#!/usr/bin/env bash
# End-to-end verification for the XDP IPv4 collector with windowed sampling and
# userspace aggregation.
#
#   probe-ebpf: Ethernet -> [802.1Q] -> IPv4 -> {AGG per-CPU map, sample ring}
#   probe:      AGG map/ring -> sample CSV (7 cols) + aggregation CSV (16 cols)
#
# The script builds a deterministic veth pair with one end in a network
# namespace, runs the probe on both ends (separate processes, `--append` on the
# second one), generates known ICMP traffic, and then asserts:
#
#   * the aggregation CSV, filtered to the relevant flow, sums to the number of
#     packets `ping` put on the wire (sigma count), with window_complete=1 and
#     first_seen/last_seen inside the emitted window;
#   * the sample CSV has exactly one row for the corresponding key -- or at most
#     two when the traffic straddled a window boundary (sampling is
#     first-packet-per-key-per-window, so it must never be one-row-per-packet).
#
# The old `events_received=N` / `rows_written=N` equalities and the exact sample
# row-count assertions are intentionally gone: the wire count now lives in the
# aggregation CSV (`count`), while the sample CSV only proves the per-window
# key deduplication.
#
# Kept from the previous version: bpftool attach/run_cnt proof, `--append`,
# 802.1Q/VLAN parsing, SIGINT shutdown, no leftover XDP/probe/netns state.
#
# Logs and CSVs are written to /workspace/verification/.
# Re-runnable: it removes its own stale probes, netns and veth first.
set -u

V=/workspace/verification
PROBE_DIR=/workspace/probe
PROBE="$PROBE_DIR/target/release/probe"
NS=aya-ns
CSV="$V/packets.csv"
AGG="$V/packets-agg.csv"
VLAN_CSV="$V/packets-vlan.csv"
VLAN_AGG="$V/packets-vlan-agg.csv"
LOG="$V/e2e.log"
BUILD_LOG="$V/build.log"
CLI_HELP="$V/cli-help.log"

# Aggregation window.  Small on purpose (fast assertions) but wide enough that
# three echo requests sent right after a window boundary fit in one window.
WINDOW_MS=4000
GRACE_MS=500
SCAN_MS=200
# Once the last packet is on the wire, the window that contains it ends within
# WINDOW_MS and is emitted after grace+scan; add a margin for the scan tick.
CLOSE_WAIT_MS=$((WINDOW_MS + GRACE_MS + SCAN_MS + 400))

SAMPLE_HEADER='ts_unix_ms,iface,src_ip,dst_ip,src_port,dst_port,protocol'
AGG_HEADER='window_start_ms,window_end_ms,iface,direction,src_prefix,dst_addr,dst_port,protocol,action,rule_id,count,first_seen_ms,last_seen_ms,sample_dropped,degraded,window_complete'
STATS_TOKENS='packets_total sample_rows events_received rows_written agg_records kernel_dropped sample_failed agg_overflow malformed late_dropped samples_deduped'
PROBE_ARGS_COMMON=(--window-ms "$WINDOW_MS" --grace-ms "$GRACE_MS" --scan-ms "$SCAN_MS"
    --src-prefix-v4 24 --src-prefix-v6 64 --flush-ms 100)

PID_A=""
PID_B=""
PID_C=""
FAIL=0
SKIP=0
CLEANED=0

log() { printf '%s %s\n' "$(date -Is)" "$*" | tee -a "$LOG"; }

check() {
    local desc=$1
    shift
    if "$@" >>"$LOG" 2>&1; then
        log "CHECK PASS: $desc"
    else
        log "CHECK FAIL: $desc"
        FAIL=$((FAIL + 1))
    fi
}

# --------------------------------------------------------------- small helpers
now_ms() { date +%s%3N; }

# Extract the numeric program id for an interface from a `bpftool net list`
# dump, e.g. "veth0(95) driver id 761" -> 761.
id_for() {
    local file=$1 dev=$2
    grep -oE "${dev}\([0-9]+\) driver id [0-9]+" "$file" | awk '{print $NF}' | head -1
}

run_cnt_of() {
    local file=$1
    grep -o 'run_cnt [0-9]*' "$file" | awk '{print $2}' | head -1
}

# First `stats:` line printed on exit, or empty when the probe died early.
stats_line() { grep -m1 '^stats: ' "$1" 2>/dev/null || true; }

# One numeric token of the `stats:` line, empty when absent.
stat_of() {
    local file=$1 field=$2
    stats_line "$file" | tr ' ' '\n' | sed -n "s/^${field}=\([0-9][0-9]*\)$/\1/p" | tail -1
}

# Same, but 0 instead of empty so `test` gets a number.
stat_n() {
    local v
    v=$(stat_of "$1" "$2")
    printf '%s' "${v:-0}"
}

# The stats line is a machine-readable contract: all tokens must be present.
check_stats_tokens() {
    local file=$1 line tok
    line=$(stats_line "$file")
    if [ -z "$line" ]; then
        echo "missing 'stats:' line in $file"
        return 1
    fi
    for tok in $STATS_TOKENS; do
        if ! printf '%s\n' "$line" | tr ' ' '\n' | grep -qx "${tok}=[0-9][0-9]*"; then
            echo "missing token '$tok' in: $line"
            return 1
        fi
    done
    return 0
}

# Wait until the wall clock is just past an aggregation window start (modulo
# < 500 ms).  Sending right after a boundary keeps the measured packets inside
# one window, which is what makes the window_complete=1 rows deterministic.
wait_window_start() {
    local deadline=$(( $(now_ms) + 20000 ))
    while [ $(( $(now_ms) % WINDOW_MS )) -ge 500 ]; do
        if [ "$(now_ms)" -ge "$deadline" ]; then
            log "WARN: window-start alignment wait timed out"
            return 1
        fi
        sleep 0.05
    done
    return 0
}

wait_until_ms() {
    local target=$1 now
    while :; do
        now=$(now_ms)
        [ "$now" -ge "$target" ] && break
        if [ $((target - now)) -gt 200 ]; then sleep 0.1; else sleep 0.02; fi
    done
}

# stop_probe PID [SIGNAL] - default SIGINT; drain/flush happens in the probe.
stop_probe() {
    local pid=${1:-}
    local sig=${2:-INT}
    [ -n "$pid" ] || return 0
    if kill -0 "$pid" 2>/dev/null; then
        kill -"$sig" "$pid" 2>/dev/null || true
        for _ in $(seq 1 60); do
            kill -0 "$pid" 2>/dev/null || break
            sleep 0.1
        done
        kill -KILL "$pid" 2>/dev/null || true
    fi
    wait "$pid" 2>/dev/null || true
}

# Kill only probes this script owns (never another script's probe): match the
# --out/--agg-out paths that only this run uses.  Matching on "--out" keeps
# `tee -a <log>` and other helper processes out of the pattern.
kill_stale_probes() {
    local base
    for base in packets.csv packets-vlan.csv packets-agg.csv packets-vlan-agg.csv; do
        pkill -f -- "--out $V/$base" 2>/dev/null || true
        pkill -f -- "--agg-out $V/$base" 2>/dev/null || true
    done
}

cleanup() {
    [ "$CLEANED" -eq 1 ] && return 0
    CLEANED=1
    log "cleanup: stopping probes and removing vlan/veth/netns"
    stop_probe "$PID_A"
    stop_probe "$PID_B"
    stop_probe "$PID_C"
    kill_stale_probes

    ip link del veth0.100 2>/dev/null || true
    ip link del veth0 2>/dev/null || true
    ip netns del "$NS" 2>/dev/null || true

    sleep 0.3
    bpftool net list >"$V/bpftool-net-final.log" 2>&1
    log "cleanup done"
}
trap cleanup EXIT INT TERM

# ------------------------------------------------------------------- CSV reads
# agg_rows FILE IFACE PREFIX DST DSTPORT PROTO WANT
#   WANT = -1: any window_complete value, 0/1: required value.
# Prints: rows sum complete_bad time_bad dir_bad action_bad "dir=<raw> act=<raw>"
# `time_bad` counts rows whose first_seen/last_seen fall outside
# [window_start_ms, window_end_ms].
agg_rows() {
    awk -F, -v iface="$2" -v prefix="$3" -v dst="$4" -v dport="$5" -v proto="$6" -v want="$7" '
        function okdir(v) { return v == "ingress" || v == 1 }
        function okact(v) { return v == "pass" || v == 0 }
        NR > 1 {
            if (iface != "" && $3 != iface) next
            if (prefix != "" && $5 != prefix) next
            if (dst != "" && $6 != dst) next
            if (dport != "" && $7 + 0 != dport + 0) next
            if (proto != "" && $8 + 0 != proto + 0) next
            n++
            sum += $11 + 0
            if (want + 0 != -1 && $16 + 0 != want + 0) cbad++
            if ($12 + 0 < $1 + 0 || $13 + 0 > $2 + 0) tbad++
            if (!okdir($4)) dbad++
            if (!okact($9)) abad++
            if (n == 1) { rawdir = $4; rawact = $9 }
        }
        END {
            printf "%d %d %d %d %d %d dir=%s act=%s\n",
                n + 0, sum + 0, cbad + 0, tbad + 0, dbad + 0, abad + 0,
                (rawdir == "" ? "?" : rawdir), (rawact == "" ? "?" : rawact)
        }' "$1"
}

check_agg_flow() {
    # desc file iface prefix dst dport proto want maxrows expsum
    local desc=$1 file=$2 iface=$3 prefix=$4 dst=$5 dport=$6 proto=$7 want=$8 maxrows=$9
    local expsum=${10}
    local out n sum cbad tbad dbad abad rest
    out=$(agg_rows "$file" "$iface" "$prefix" "$dst" "$dport" "$proto" "$want")
    read -r n sum cbad tbad dbad abad rest <<<"$out"
    log "agg($desc): rows=${n:-?} sum=${sum:-?} complete_bad=${cbad:-?} time_bad=${tbad:-?} dir_bad=${dbad:-?} action_bad=${abad:-?} ${rest:-}"
    check "$desc: agg sigma(count)=$expsum" test "${sum:-0}" -eq "$expsum"
    check "$desc: agg has 1..$maxrows row(s) (got ${n:-0})" \
        test "${n:-0}" -ge 1 -a "${n:-0}" -le "$maxrows"
    check "$desc: agg window_complete matches want=$want (bad ${cbad:-?})" \
        test "${cbad:-1}" -eq 0
    check "$desc: agg first/last inside window (bad ${tbad:-?})" test "${tbad:-1}" -eq 0
    check "$desc: agg direction=ingress action=pass (bad ${dbad:-1}/${abad:-1})" \
        test "${dbad:-1}" -eq 0 -a "${abad:-1}" -eq 0
}

# sample_count FILE IFACE SRC DST PROTO -> number of matching sample rows.
sample_count() {
    awk -F, -v iface="$2" -v src="$3" -v dst="$4" -v proto="$5" '
        NR > 1 {
            if (iface != "" && $2 != iface) next
            if (src != "" && $3 != src) next
            if (dst != "" && $4 != dst) next
            if (proto != "" && $7 + 0 != proto + 0) next
            n++
        }
        END { print n + 0 }' "$1"
}

check_sample_flow() {
    # desc file iface src dst proto maxrows
    local desc=$1 file=$2 iface=$3 src=$4 dst=$5 proto=$6 maxrows=$7
    local n
    n=$(sample_count "$file" "$iface" "$src" "$dst" "$proto")
    log "sample($desc): rows=$n (iface=$iface $src -> $dst protocol=$proto)"
    check "$desc: sample has 1..$maxrows row(s) for one key/window (got $n)" \
        test "$n" -ge 1 -a "$n" -le "$maxrows"
}

# Distinct values in a CSV column, ignoring the header.
distinct_col() {
    awk -F, -v col="$2" 'NR > 1 { print $col }' "$1" | sort -u | wc -l
}

# ======================================================================= start
: >"$LOG"
log "=== e2e start (IPv4, windowed sampling + aggregation) ==="

log "building probe: cd $PROBE_DIR && cargo build --release"
if ! ( cd "$PROBE_DIR" && cargo build --release ) >"$BUILD_LOG" 2>&1; then
    log "FATAL: cargo build --release failed (see $BUILD_LOG)"
    tail -n 40 "$BUILD_LOG" | tee -a "$LOG"
    exit 1
fi
log "build OK"
if [ ! -x "$PROBE" ]; then
    log "FATAL: $PROBE missing after build"
    exit 1
fi

# CLI preflight: the assertions below depend on the aggregation flags.
"$PROBE" --help >"$CLI_HELP" 2>&1 || true
for flag in --agg-out --window-ms --grace-ms --scan-ms --src-prefix-v4 --src-prefix-v6; do
    check "CLI advertises $flag" grep -q -- "$flag" "$CLI_HELP"
done

# ---------------------------------------------------------------- environment
log "cleaning previous state"
kill_stale_probes
ip link del veth0.100 2>/dev/null || true
ip link del veth0 2>/dev/null || true
ip netns del "$NS" 2>/dev/null || true

# `bpftool prog show` only reports run_cnt when per-program stats are enabled;
# this WSL2 kernel defaults to 0, and the run_cnt assertions below would compare
# 0 with 0 and fail for the wrong reason.
sysctl -qw kernel.bpf_stats_enabled=1 2>>"$LOG" || true

log "creating netns $NS and veth pair"
ip netns add "$NS" || { log "FATAL: ip netns add failed"; exit 1; }
ip link add veth0 type veth peer name veth1 || { log "FATAL: veth add failed"; exit 1; }
ip link set veth1 netns "$NS" || exit 1

# This script asserts ICMPv4 counts, so IPv6 is switched off on the veth pair
# (the kernel enables it by default, which would add ND/MLD rows).  IPv6 has
# its own end-to-end script: verification/run_e2e_ipv6.sh.
echo 1 >/proc/sys/net/ipv6/conf/veth0/disable_ipv6 2>>"$LOG"
ip netns exec "$NS" sh -c 'echo 1 >/proc/sys/net/ipv6/conf/veth1/disable_ipv6' 2>>"$LOG"

ip addr add 10.99.0.1/24 dev veth0 || exit 1
ip link set veth0 up || exit 1
ip netns exec "$NS" ip link set lo up || exit 1
ip netns exec "$NS" ip addr add 10.99.0.2/24 dev veth1 || exit 1
ip netns exec "$NS" ip link set veth1 up || exit 1

ip -br addr show veth0 | tee -a "$LOG"
ip netns exec "$NS" ip -br addr show veth1 | tee -a "$LOG"

# --------------------------------------------------------------------- phase A
# XDP on veth0: ingress traffic comes from the namespace end.  Warm the ARP
# cache *before* the probe attaches so the measured echo requests leave
# immediately after the window-start wait instead of waiting for ARP.
phase_a() {
    local logf="$V/probe-veth0.log"
    log "phase A: probe on veth0, ping 10.99.0.1 from $NS (expect 10.99.0.2 -> 10.99.0.1)"
    ip netns exec "$NS" ping -c 1 -W 1 10.99.0.1 >"$V/ping-preheat-a.log" 2>&1 || true

    rm -f "$CSV" "$AGG"
    "$PROBE" --iface veth0 --out "$CSV" --agg-out "$AGG" \
        "${PROBE_ARGS_COMMON[@]}" >"$logf" 2>&1 &
    PID_A=$!
    sleep 1
    if ! kill -0 "$PID_A" 2>/dev/null; then
        log "FATAL: probe A exited early"
        cat "$logf" | tee -a "$LOG"
        PID_A=""
        FAIL=$((FAIL + 1))
        return 0
    fi

    bpftool net list >"$V/bpftool-net-A.log" 2>&1
    ID_A=$(id_for "$V/bpftool-net-A.log" veth0)
    bpftool prog show id "${ID_A:-0}" >"$V/bpftool-prog-A-before.log" 2>&1
    log "phase A program id: ${ID_A:-<missing>}"
    cat "$V/bpftool-net-A.log" | tee -a "$LOG"

    wait_window_start
    ip netns exec "$NS" ping -c 3 -i 0.2 -W 1 10.99.0.1 >"$V/ping-ns-to-host.log" 2>&1
    PING_A_RC=$?
    log "ping ns->host rc=$PING_A_RC"
    cat "$V/ping-ns-to-host.log" | tee -a "$LOG"
    # Hold the probe until the window that saw the traffic has been closed and
    # written with window_complete=1.
    wait_until_ms $(( $(now_ms) + CLOSE_WAIT_MS ))

    bpftool prog show id "${ID_A:-0}" >"$V/bpftool-prog-A-after.log" 2>&1
    cat "$V/bpftool-prog-A-after.log" | tee -a "$LOG"
    RUN_A_BEFORE=$(run_cnt_of "$V/bpftool-prog-A-before.log")
    RUN_A_AFTER=$(run_cnt_of "$V/bpftool-prog-A-after.log")

    stop_probe "$PID_A" INT
    PID_A=""
    cat "$logf" | tee -a "$LOG"

    check "phase A ping 3/3 without loss" grep -q '3 received, 0% packet loss' "$V/ping-ns-to-host.log"
    check "phase A sample CSV header is the 7-column contract" \
        test "$(head -n 1 "$CSV")" = "$SAMPLE_HEADER"
    check "phase A agg CSV header is the 16-column contract" \
        test "$(head -n 1 "$AGG")" = "$AGG_HEADER"
    check_agg_flow "phase A veth0" "$AGG" veth0 10.99.0.0/24 10.99.0.1 0 1 1 2 3
    check_sample_flow "phase A veth0" "$CSV" veth0 10.99.0.2 10.99.0.1 1 2
    check "phase A stats line has all tokens" check_stats_tokens "$logf"
    check "phase A no kernel drops" test "$(stat_n "$logf" kernel_dropped)" -eq 0
    check "phase A no malformed records" test "$(stat_n "$logf" malformed)" -eq 0
    check "phase A no late drops" test "$(stat_n "$logf" late_dropped)" -eq 0
    check "phase A no agg overflow" test "$(stat_n "$logf" agg_overflow)" -eq 0
    check "phase A sample_rows is 1..2 (got $(stat_n "$logf" sample_rows))" \
        test "$(stat_n "$logf" sample_rows)" -ge 1 -a "$(stat_n "$logf" sample_rows)" -le 2
    check "phase A at least one agg record was emitted (got $(stat_n "$logf" agg_records))" \
        test "$(stat_n "$logf" agg_records)" -ge 1
    check "phase A bpftool shows program attached to veth0" test -n "$ID_A"
    check "phase A run_cnt grew (${RUN_A_BEFORE:-0} -> ${RUN_A_AFTER:-0})" \
        test "${RUN_A_AFTER:-0}" -gt "${RUN_A_BEFORE:-0}"
}

# --------------------------------------------------------------------- phase B
# XDP on veth1 (inside the namespace); append to the same CSVs: reverse
# direction + `--append` restart semantics.  This phase is also the exit
# "partial flush" scenario: SIGINT arrives while the window is still open, so
# the row(s) must be emitted with window_complete=0.
phase_b() {
    local logf="$V/probe-veth1.log"
    log "phase B: probe on veth1 (inside $NS, --append), ping 10.99.0.2 from host (expect 10.99.0.1 -> 10.99.0.2)"
    ping -c 1 -W 1 10.99.0.2 >"$V/ping-preheat-b.log" 2>&1 || true

    ip netns exec "$NS" "$PROBE" --iface veth1 --out "$CSV" --agg-out "$AGG" --append \
        "${PROBE_ARGS_COMMON[@]}" >"$logf" 2>&1 &
    PID_B=$!
    sleep 1
    if ! kill -0 "$PID_B" 2>/dev/null; then
        log "FATAL: probe B exited early"
        cat "$logf" | tee -a "$LOG"
        PID_B=""
        FAIL=$((FAIL + 1))
        return 0
    fi

    ip netns exec "$NS" bpftool net list >"$V/bpftool-net-B.log" 2>&1
    ID_B=$(id_for "$V/bpftool-net-B.log" veth1)
    bpftool prog show id "${ID_B:-0}" >"$V/bpftool-prog-B-before.log" 2>&1
    log "phase B program id: ${ID_B:-<missing>}"
    cat "$V/bpftool-net-B.log" | tee -a "$LOG"

    wait_window_start
    # The probe listens on veth1 *inside* the namespace; the echo requests must
    # be sent from the host side so they actually cross veth1 (pinging
    # 10.99.0.2 from inside the namespace would be local delivery and the XDP
    # program would never run).
    ping -c 3 -i 0.01 -W 1 10.99.0.2 >"$V/ping-host-to-ns.log" 2>&1
    PING_B_RC=$?
    log "ping host->ns rc=$PING_B_RC"
    cat "$V/ping-host-to-ns.log" | tee -a "$LOG"
    # SIGINT immediately: the three packets are ~20 ms apart, so their window is
    # still open and the exit path has to flush it as partial.
    bpftool prog show id "${ID_B:-0}" >"$V/bpftool-prog-B-after.log" 2>&1
    RUN_B_BEFORE=$(run_cnt_of "$V/bpftool-prog-B-before.log")
    RUN_B_AFTER=$(run_cnt_of "$V/bpftool-prog-B-after.log")

    stop_probe "$PID_B" INT
    PID_B=""
    cat "$logf" | tee -a "$LOG"

    check "phase B ping 3/3 without loss" grep -q '3 received, 0% packet loss' "$V/ping-host-to-ns.log"
    check "phase B partial flush wrote window_complete=0 row(s)" \
        check_agg_flow "phase B veth1" "$AGG" veth1 10.99.0.0/24 10.99.0.2 0 1 0 2 3
    check_sample_flow "phase B veth1" "$CSV" veth1 10.99.0.1 10.99.0.2 1 2
    check "phase B stats line has all tokens" check_stats_tokens "$logf"
    check "phase B no kernel drops" test "$(stat_n "$logf" kernel_dropped)" -eq 0
    check "phase B no malformed records" test "$(stat_n "$logf" malformed)" -eq 0
    check "phase B no late drops" test "$(stat_n "$logf" late_dropped)" -eq 0
    check "phase B no agg overflow" test "$(stat_n "$logf" agg_overflow)" -eq 0
    check "phase B bpftool shows program attached inside netns" test -n "$ID_B"
    check "phase B run_cnt grew (${RUN_B_BEFORE:-0} -> ${RUN_B_AFTER:-0})" \
        test "${RUN_B_AFTER:-0}" -gt "${RUN_B_BEFORE:-0}"

    # --append semantics: one header per file, and phase A rows survive.
    check "append kept exactly one sample CSV header" \
        test "$(grep -c '^ts_unix_ms,iface' "$CSV")" -eq 1
    check "append kept exactly one agg CSV header" \
        test "$(grep -c '^window_start_ms,window_end_ms' "$AGG")" -eq 1
    check "sample CSV contains both interfaces" test "$(distinct_col "$CSV" 2)" -eq 2
    check "agg CSV contains both interfaces" test "$(distinct_col "$AGG" 3)" -eq 2
    check "phase A sample row survived --append" \
        test "$(sample_count "$CSV" veth0 10.99.0.2 10.99.0.1 1)" -ge 1

    # Sample timestamps are wall-clock milliseconds: first row must be recent.
    local now first delta
    now=$(now_ms)
    first=$(awk -F, 'NR==2{print $1}' "$CSV")
    if [ -n "$first" ]; then
        delta=$((now - first))
        check "sample ts_unix_ms is current (delta ${delta} ms)" \
            test "$delta" -ge 0 -a "$delta" -lt 300000
    else
        check "sample ts_unix_ms is current" false
    fi

    log "sample CSV head:"
    head -n 4 "$CSV" | tee -a "$LOG"
    log "agg CSV head:"
    head -n 4 "$AGG" | tee -a "$LOG"
}

# --------------------------------------------------------------------- phase C
# Optional 802.1Q parsing: ping over a VLAN sub-interface on the same veth pair.
phase_c() {
    local logf="$V/probe-vlan.log"
    log "phase C: VLAN 100 over veth, probe on veth0 (expect 10.100.0.2 -> 10.100.0.1)"
    ip link add link veth0 name veth0.100 type vlan id 100 2>>"$LOG"
    # Keep IPv6 out of the VLAN CSV as well.
    echo 1 >/proc/sys/net/ipv6/conf/veth0.100/disable_ipv6 2>>"$LOG"
    ip addr add 10.100.0.1/24 dev veth0.100 2>>"$LOG"
    ip link set veth0.100 up 2>>"$LOG"
    ip netns exec "$NS" ip link add link veth1 name veth1.100 type vlan id 100 2>>"$LOG"
    ip netns exec "$NS" sh -c 'echo 1 >/proc/sys/net/ipv6/conf/veth1.100/disable_ipv6' 2>>"$LOG"
    ip netns exec "$NS" ip addr add 10.100.0.2/24 dev veth1.100 2>>"$LOG"
    ip netns exec "$NS" ip link set veth1.100 up 2>>"$LOG"

    ip netns exec "$NS" ping -c 1 -W 1 10.100.0.1 >"$V/ping-preheat-vlan.log" 2>&1 || true

    rm -f "$VLAN_CSV" "$VLAN_AGG"
    "$PROBE" --iface veth0 --out "$VLAN_CSV" --agg-out "$VLAN_AGG" \
        "${PROBE_ARGS_COMMON[@]}" >"$logf" 2>&1 &
    PID_C=$!
    sleep 1
    if ! kill -0 "$PID_C" 2>/dev/null; then
        log "FATAL: VLAN probe exited early"
        cat "$logf" | tee -a "$LOG"
        PID_C=""
        FAIL=$((FAIL + 1))
        return 0
    fi

    wait_window_start
    ip netns exec "$NS" ping -c 3 -i 0.2 -W 1 10.100.0.1 >"$V/ping-vlan.log" 2>&1
    log "ping VLAN rc=$?"
    cat "$V/ping-vlan.log" | tee -a "$LOG"
    wait_until_ms $(( $(now_ms) + CLOSE_WAIT_MS ))

    bpftool net list >"$V/bpftool-net-C.log" 2>&1
    ID_C=$(id_for "$V/bpftool-net-C.log" veth0)
    stop_probe "$PID_C" INT
    PID_C=""
    cat "$logf" | tee -a "$LOG"

    check "VLAN ping 3/3 without loss" grep -q '3 received, 0% packet loss' "$V/ping-vlan.log"
    check "VLAN sample CSV header is the 7-column contract" \
        test "$(head -n 1 "$VLAN_CSV")" = "$SAMPLE_HEADER"
    check "VLAN agg CSV header is the 16-column contract" \
        test "$(head -n 1 "$VLAN_AGG")" = "$AGG_HEADER"
    check_agg_flow "phase C vlan100" "$VLAN_AGG" veth0 10.100.0.0/24 10.100.0.1 0 1 1 2 3
    check_sample_flow "phase C vlan100" "$VLAN_CSV" veth0 10.100.0.2 10.100.0.1 1 2
    check "VLAN stats line has all tokens" check_stats_tokens "$logf"
    check "VLAN no kernel drops" test "$(stat_n "$logf" kernel_dropped)" -eq 0
    check "VLAN no malformed records" test "$(stat_n "$logf" malformed)" -eq 0
    check "VLAN no late drops" test "$(stat_n "$logf" late_dropped)" -eq 0
    check "VLAN no agg overflow" test "$(stat_n "$logf" agg_overflow)" -eq 0
}

phase_a
phase_b
phase_c

# ---------------------------------------------------------------------- cleanup
log "explicit cleanup before final checks"
cleanup

FINAL_XDP=$(awk '/^xdp:/{in_xdp=1;next} /^[a-z]/{in_xdp=0} in_xdp && NF {print}' "$V/bpftool-net-final.log")
check "no XDP attachment left after cleanup" test -z "$FINAL_XDP"
check "no probe process left for this script's CSVs" \
    test -z "$(pgrep -f -- "--agg-out $V/packets" 2>/dev/null || true)"
check "netns $NS removed" test -z "$(ip netns list 2>/dev/null | awk -v ns="$NS" '$1==ns')"
check "veth0 removed" test -z "$(ip link show veth0 2>/dev/null || true)"

log "=== summary ==="
log "phase A agg/sample logs: $V/probe-veth0.log, $V/packets-agg.csv"
log "phase B agg/sample logs: $V/probe-veth1.log, $V/packets.csv"
log "phase C agg/sample logs: $V/probe-vlan.log, $V/packets-vlan-agg.csv"
log "final bpftool net list:"
cat "$V/bpftool-net-final.log" | tee -a "$LOG"

if [ "$FAIL" -eq 0 ] && [ "$SKIP" -eq 0 ]; then
    log "RESULT: PASS"
elif [ "$FAIL" -eq 0 ]; then
    log "RESULT: PASS ($SKIP check(s) skipped)"
else
    log "RESULT: FAIL ($FAIL check(s) failed, $SKIP skipped)"
fi
# Not `exit "$FAIL"`: a process status is only 8 bits wide, so a failure count
# of 256 (or any multiple of it) exits 0 and reports a failing suite as green.
[ "$FAIL" -eq 0 ] || exit 1
