#!/usr/bin/env bash
# End-to-end verification for the XDP IPv4/IPv6 collector with windowed
# sampling and userspace aggregation.
#
#   probe-ebpf: Ethernet -> [802.1Q] -> IPv4 | IPv6 -> {AGG per-CPU map, sample ring}
#   probe:      AGG map/ring -> sample CSV (7 cols) + aggregation CSV (16 cols)
#
# Phases:
#   AB  probes on both ends of a veth pair (second one with --append, writing
#       the same CSVs); ping -6 in both directions over a ULA prefix.  SIGINT
#       stops the veth0 probe, SIGTERM the netns probe, so both shutdown paths
#       are exercised.
#   C   IPv6 over VLAN 100: proves the 0x8100 -> 0x86DD dispatch.
#   D   IPv4 + IPv6 in one run: both address families coexist in one CSV.
#
# Assertions use the aggregation CSV: sigma(count) for the ULA flow is 6 per
# probe / 12 in total.  Each probe only sees its own ingress direction, but
# that direction carries BOTH the 3 echo requests of the ping sent from the far
# side AND the 3 echo replies to the ping sent from its own side (echo
# request/reply share the same aggregation key because ICMPv6 ports are 0).
# 3 + 3 = 6 per probe.  The sample CSV must hold one row per key and window
# (two only if the traffic straddled a window boundary), never one row per
# packet.
#
# veth links automatically get IPv6 link-local addresses, so ND/MLD/ICMPv6
# control packets (fe80::, ff02::) are expected in both CSVs.  Every assertion
# is therefore filtered by the ULA prefix instead of counting total rows.
#
# Re-runnable: it removes stale probes, netns and veth first, and cleans up on
# exit.  Logs/CSVs go to /workspace/verification/ and are prefixed "ipv6-".
set -u

V=/workspace/verification
PROBE_DIR=/workspace/probe
PROBE="$PROBE_DIR/target/release/probe"
NS=aya-ns
CSV="$V/ipv6-packets.csv"
AGG="$V/ipv6-packets-agg.csv"
VLAN_CSV="$V/ipv6-vlan.csv"
VLAN_AGG="$V/ipv6-vlan-agg.csv"
MIXED_CSV="$V/ipv6-mixed.csv"
MIXED_AGG="$V/ipv6-mixed-agg.csv"
LOG="$V/ipv6-e2e.log"
BUILD_LOG="$V/ipv6-build.log"
CLI_HELP="$V/ipv6-cli-help.log"

# Aggregation window.  Small on purpose (fast assertions) but wide enough that
# three echo requests sent right after a window boundary fit in one window.
WINDOW_MS=4000
GRACE_MS=500
SCAN_MS=200
CLOSE_WAIT_MS=$((WINDOW_MS + GRACE_MS + SCAN_MS + 400))

SAMPLE_HEADER='ts_unix_ms,iface,src_ip,dst_ip,src_port,dst_port,protocol'
AGG_HEADER='window_start_ms,window_end_ms,iface,direction,src_prefix,dst_addr,dst_port,protocol,action,rule_id,count,first_seen_ms,last_seen_ms,sample_dropped,degraded,window_complete'
STATS_TOKENS='packets_total sample_rows events_received rows_written agg_records kernel_dropped sample_failed agg_overflow malformed late_dropped samples_deduped'
PROBE_ARGS_COMMON=(--window-ms "$WINDOW_MS" --grace-ms "$GRACE_MS" --scan-ms "$SCAN_MS"
    --src-prefix-v4 24 --src-prefix-v6 64 --flush-ms 100)

PID_A=""
PID_B=""
PID_C=""
PID_D=""
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

id_for() {
    local file=$1 dev=$2
    grep -oE "${dev}\([0-9]+\) driver id [0-9]+" "$file" | awk '{print $NF}' | head -1
}

run_cnt_of() {
    local file=$1
    grep -o 'run_cnt [0-9]*' "$file" | awk '{print $2}' | head -1
}

stats_line() { grep -m1 '^stats: ' "$1" 2>/dev/null || true; }

stat_of() {
    local file=$1 field=$2
    stats_line "$file" | tr ' ' '\n' | sed -n "s/^${field}=\([0-9][0-9]*\)$/\1/p" | tail -1
}

stat_n() {
    local v
    v=$(stat_of "$1" "$2")
    printf '%s' "${v:-0}"
}

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

# stop_probe PID [SIGNAL] - default SIGINT; the probe drains and flushes.
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
    for base in ipv6-packets.csv ipv6-vlan.csv ipv6-mixed.csv \
        ipv6-packets-agg.csv ipv6-vlan-agg.csv ipv6-mixed-agg.csv; do
        pkill -f -- "--out $V/$base" 2>/dev/null || true
        pkill -f -- "--agg-out $V/$base" 2>/dev/null || true
    done
}

cleanup() {
    [ "$CLEANED" -eq 1 ] && return 0
    CLEANED=1
    log "cleanup: stopping probes and removing vlan/veth/netns"
    stop_probe "$PID_A"
    stop_probe "$PID_B" TERM
    stop_probe "$PID_C"
    stop_probe "$PID_D"
    kill_stale_probes

    ip link del veth0.100 2>/dev/null || true
    ip link del veth0 2>/dev/null || true
    ip netns del "$NS" 2>/dev/null || true

    sleep 0.3
    bpftool net list >"$V/ipv6-bpftool-net-final.log" 2>&1
    log "cleanup done"
}
trap cleanup EXIT INT TERM

# ------------------------------------------------------------------- CSV reads
# agg_rows FILE IFACE PREFIX DST DSTPORT PROTO WANT
# Prints: rows sum complete_bad time_bad dir_bad action_bad "dir=<raw> act=<raw>"
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

distinct_col() {
    awk -F, -v col="$2" 'NR > 1 { print $col }' "$1" | sort -u | wc -l
}

# ======================================================================= start
: >"$LOG"
log "=== IPv6 e2e start (IPv4+IPv6, windowed sampling + aggregation) ==="

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

# IPv6 is enabled in this image, but make the script self-sufficient: turn it on
# for the container netns and for links created from now on.
sysctl -qw net.ipv6.conf.all.disable_ipv6=0 2>>"$LOG" || true
sysctl -qw net.ipv6.conf.default.disable_ipv6=0 2>>"$LOG" || true
sysctl -qw kernel.bpf_stats_enabled=1 2>>"$LOG" || true
check "IPv6 stack usable (ping -6 ::1)" ping -6 -c 1 -W 1 ::1

log "creating netns $NS and veth pair"
ip netns add "$NS" || { log "FATAL: ip netns add failed"; exit 1; }
ip link add veth0 type veth peer name veth1 || { log "FATAL: veth add failed"; exit 1; }
ip link set veth1 netns "$NS" || exit 1

echo 0 >/proc/sys/net/ipv6/conf/veth0/disable_ipv6 2>>"$LOG" || true
ip netns exec "$NS" sh -c 'echo 0 >/proc/sys/net/ipv6/conf/veth1/disable_ipv6' 2>>"$LOG" || true

ip addr add 10.99.0.1/24 dev veth0 || exit 1
ip -6 addr add fd00:99::1/64 dev veth0 nodad || exit 1
ip link set veth0 up || exit 1
ip netns exec "$NS" ip link set lo up || exit 1
ip netns exec "$NS" ip addr add 10.99.0.2/24 dev veth1 || exit 1
ip netns exec "$NS" ip -6 addr add fd00:99::2/64 dev veth1 nodad || exit 1
ip netns exec "$NS" ip link set veth1 up || exit 1

ip -br addr show veth0 | tee -a "$LOG"
ip netns exec "$NS" ip -br addr show veth1 | tee -a "$LOG"

# ------------------------------------------------------------------ phase AB
# Both ends capture simultaneously into the same CSVs; the second process
# appends.  ND is pre-warmed before the probes attach so the measured echo
# requests are not delayed by neighbour discovery.
phase_ab() {
    local loga="$V/ipv6-probe-veth0.log" logb="$V/ipv6-probe-veth1.log"
    log "phase AB: probes on veth0 and veth1 (inside $NS, --append), ping -6 both ways"
    ip netns exec "$NS" ping -6 -c 1 -W 1 fd00:99::1 >"$V/ipv6-preheat-a.log" 2>&1 || true
    ping -6 -c 1 -W 1 fd00:99::2 >"$V/ipv6-preheat-b.log" 2>&1 || true

    rm -f "$CSV" "$AGG"
    "$PROBE" --iface veth0 --out "$CSV" --agg-out "$AGG" \
        "${PROBE_ARGS_COMMON[@]}" >"$loga" 2>&1 &
    PID_A=$!
    sleep 1
    if ! kill -0 "$PID_A" 2>/dev/null; then
        log "FATAL: probe A exited early"
        cat "$loga" | tee -a "$LOG"
        PID_A=""
        FAIL=$((FAIL + 1))
        return 0
    fi

    ip netns exec "$NS" "$PROBE" --iface veth1 --out "$CSV" --agg-out "$AGG" --append \
        "${PROBE_ARGS_COMMON[@]}" >"$logb" 2>&1 &
    PID_B=$!
    sleep 1
    if ! kill -0 "$PID_B" 2>/dev/null; then
        log "FATAL: probe B exited early"
        cat "$logb" | tee -a "$LOG"
        PID_B=""
        FAIL=$((FAIL + 1))
        return 0
    fi

    bpftool net list >"$V/ipv6-bpftool-net-a.log" 2>&1
    ID_A=$(id_for "$V/ipv6-bpftool-net-a.log" veth0)
    log "veth0 program id: ${ID_A:-<missing>}"
    bpftool prog show id "${ID_A:-0}" >"$V/ipv6-bpftool-prog-a-before.log" 2>&1

    ip netns exec "$NS" bpftool net list >"$V/ipv6-bpftool-net-b.log" 2>&1
    ID_B=$(id_for "$V/ipv6-bpftool-net-b.log" veth1)
    log "veth1 program id: ${ID_B:-<missing>}"
    ip netns exec "$NS" bpftool prog show id "${ID_B:-0}" >"$V/ipv6-bpftool-prog-b-before.log" 2>&1

    cat "$V/ipv6-bpftool-net-a.log" | tee -a "$LOG"
    cat "$V/ipv6-bpftool-net-b.log" | tee -a "$LOG"

    wait_window_start
    ip netns exec "$NS" ping -6 -c 3 -i 0.2 -W 1 fd00:99::1 >"$V/ipv6-ping-ns-to-host.log" 2>&1
    log "ping -6 ns->host rc=$?"
    cat "$V/ipv6-ping-ns-to-host.log" | tee -a "$LOG"
    ping -6 -c 3 -i 0.2 -W 1 fd00:99::2 >"$V/ipv6-ping-host-to-ns.log" 2>&1
    log "ping -6 host->ns rc=$?"
    cat "$V/ipv6-ping-host-to-ns.log" | tee -a "$LOG"
    # Hold both probes until their window is closed (window_complete=1).
    wait_until_ms $(( $(now_ms) + CLOSE_WAIT_MS ))

    bpftool prog show id "${ID_A:-0}" >"$V/ipv6-bpftool-prog-a-after.log" 2>&1
    ip netns exec "$NS" bpftool prog show id "${ID_B:-0}" >"$V/ipv6-bpftool-prog-b-after.log" 2>&1
    RUN_A_BEFORE=$(run_cnt_of "$V/ipv6-bpftool-prog-a-before.log")
    RUN_A_AFTER=$(run_cnt_of "$V/ipv6-bpftool-prog-a-after.log")
    RUN_B_BEFORE=$(run_cnt_of "$V/ipv6-bpftool-prog-b-before.log")
    RUN_B_AFTER=$(run_cnt_of "$V/ipv6-bpftool-prog-b-after.log")

    # SIGINT for the veth0 probe, SIGTERM for the netns probe: both must drain
    # and flush before detaching.
    stop_probe "$PID_A" INT
    PID_A=""
    stop_probe "$PID_B" TERM
    PID_B=""

    cat "$loga" | tee -a "$LOG"
    cat "$logb" | tee -a "$LOG"

    check "phase A ping -6 3/3 without loss" \
        grep -q '3 received, 0% packet loss' "$V/ipv6-ping-ns-to-host.log"
    check "phase B ping -6 3/3 without loss" \
        grep -q '3 received, 0% packet loss' "$V/ipv6-ping-host-to-ns.log"
    check "phase AB sample CSV header is the 7-column contract" \
        test "$(head -n 1 "$CSV")" = "$SAMPLE_HEADER"
    check "phase AB agg CSV header is the 16-column contract" \
        test "$(head -n 1 "$AGG")" = "$AGG_HEADER"
    check_agg_flow "phase AB veth0" "$AGG" veth0 fd00:99::/64 fd00:99::1 0 58 1 2 6
    check_agg_flow "phase AB veth1" "$AGG" veth1 fd00:99::/64 fd00:99::2 0 58 1 2 6
    check_sample_flow "phase AB veth0" "$CSV" veth0 fd00:99::2 fd00:99::1 58 2
    check_sample_flow "phase AB veth1" "$CSV" veth1 fd00:99::1 fd00:99::2 58 2
    # Sum both explicit directions instead of wildcarding dst_addr: ND/MLD
    # packets with a ULA source (e.g. router solicitation to ff02::2) also carry
    # protocol 58 and src_prefix fd00:99::/64, so a wildcard would over-count.
    AB_TOTAL=$(( $(agg_rows "$AGG" veth0 fd00:99::/64 fd00:99::1 0 58 1 | awk '{print $2}') \
        + $(agg_rows "$AGG" veth1 fd00:99::/64 fd00:99::2 0 58 1 | awk '{print $2}') ))
    check "phase AB aggregate total for both probes is 12 (got $AB_TOTAL)" \
        test "$AB_TOTAL" -eq 12
    check "phase AB stats line has all tokens (probe A)" check_stats_tokens "$loga"
    check "phase AB stats line has all tokens (probe B)" check_stats_tokens "$logb"
    check "phase AB no kernel drops (probe A)" test "$(stat_n "$loga" kernel_dropped)" -eq 0
    check "phase AB no kernel drops (probe B)" test "$(stat_n "$logb" kernel_dropped)" -eq 0
    check "phase AB no malformed records (probe A)" test "$(stat_n "$loga" malformed)" -eq 0
    check "phase AB no malformed records (probe B)" test "$(stat_n "$logb" malformed)" -eq 0
    check "phase AB no late drops (probe A)" test "$(stat_n "$loga" late_dropped)" -eq 0
    check "phase AB no late drops (probe B)" test "$(stat_n "$logb" late_dropped)" -eq 0
    check "phase AB no agg overflow (probe A)" test "$(stat_n "$loga" agg_overflow)" -eq 0
    check "phase AB no agg overflow (probe B)" test "$(stat_n "$logb" agg_overflow)" -eq 0
    check "append kept exactly one sample CSV header" \
        test "$(grep -c '^ts_unix_ms,iface' "$CSV")" -eq 1
    check "append kept exactly one agg CSV header" \
        test "$(grep -c '^window_start_ms,window_end_ms' "$AGG")" -eq 1
    check "sample CSV contains both interfaces" test "$(distinct_col "$CSV" 2)" -eq 2
    check "agg CSV contains both interfaces" test "$(distinct_col "$AGG" 3)" -eq 2
    check "veth0 program attached (id ${ID_A:-none})" test -n "$ID_A"
    check "veth1 program attached inside netns (id ${ID_B:-none})" test -n "$ID_B"
    check "veth0 run_cnt grew (${RUN_A_BEFORE:-0} -> ${RUN_A_AFTER:-0})" \
        test "${RUN_A_AFTER:-0}" -gt "${RUN_A_BEFORE:-0}"
    check "veth1 run_cnt grew (${RUN_B_BEFORE:-0} -> ${RUN_B_AFTER:-0})" \
        test "${RUN_B_AFTER:-0}" -gt "${RUN_B_BEFORE:-0}"

    # Formatting sanity: link-local bytes must be formatted as IPv6 (they start
    # with 0xfe 0x80, which would decode as 254.128.x.x if family handling were
    # wrong) and no IPv4-mapped addresses may leak into the IPv6 column.
    check "no fe80 bytes decoded as IPv4 (254.128.x.x)" \
        test "$(grep -c '254\.128\.' "$CSV" || true)" -eq 0
    check "no IPv4-mapped ::ffff: addresses in sample CSV" \
        test "$(grep -c '::ffff:' "$CSV" || true)" -eq 0

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

    log "phase AB agg CSV (ULA rows):"
    awk -F, 'NR==1 || ($5 ~ /fd00:/)' "$AGG" | head -n 6 | tee -a "$LOG"
}

# ------------------------------------------------------------------- phase C
# IPv6 over 802.1Q: the innermost ethertype 0x86DD follows the VLAN tag.
phase_c() {
    local logf="$V/ipv6-probe-vlan.log"
    log "phase C: IPv6 over VLAN 100 (expect 0x8100 -> 0x86DD)"
    ip link add link veth0 name veth0.100 type vlan id 100 2>>"$LOG"
    ip -6 addr add fd00:100::1/64 dev veth0.100 nodad 2>>"$LOG"
    ip link set veth0.100 up 2>>"$LOG"
    ip netns exec "$NS" ip link add link veth1 name veth1.100 type vlan id 100 2>>"$LOG"
    ip netns exec "$NS" ip -6 addr add fd00:100::2/64 dev veth1.100 nodad 2>>"$LOG"
    ip netns exec "$NS" ip link set veth1.100 up 2>>"$LOG"

    ip netns exec "$NS" ping -6 -c 1 -W 1 fd00:100::1 >"$V/ipv6-preheat-vlan.log" 2>&1 || true

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
    ip netns exec "$NS" ping -6 -c 3 -i 0.2 -W 1 fd00:100::1 >"$V/ipv6-ping-vlan.log" 2>&1
    log "ping -6 VLAN rc=$?"
    cat "$V/ipv6-ping-vlan.log" | tee -a "$LOG"
    wait_until_ms $(( $(now_ms) + CLOSE_WAIT_MS ))

    bpftool net list >"$V/ipv6-bpftool-net-c.log" 2>&1
    ID_C=$(id_for "$V/ipv6-bpftool-net-c.log" veth0)
    stop_probe "$PID_C" INT
    PID_C=""
    cat "$logf" | tee -a "$LOG"

    check "VLAN ping -6 3/3 without loss" grep -q '3 received, 0% packet loss' "$V/ipv6-ping-vlan.log"
    check "VLAN sample CSV header is the 7-column contract" \
        test "$(head -n 1 "$VLAN_CSV")" = "$SAMPLE_HEADER"
    check "VLAN agg CSV header is the 16-column contract" \
        test "$(head -n 1 "$VLAN_AGG")" = "$AGG_HEADER"
    check_agg_flow "phase C vlan100" "$VLAN_AGG" veth0 fd00:100::/64 fd00:100::1 0 58 1 2 3
    check_sample_flow "phase C vlan100" "$VLAN_CSV" veth0 fd00:100::2 fd00:100::1 58 2
    check "VLAN stats line has all tokens" check_stats_tokens "$logf"
    check "VLAN no kernel drops" test "$(stat_n "$logf" kernel_dropped)" -eq 0
    check "VLAN no malformed records" test "$(stat_n "$logf" malformed)" -eq 0
    check "VLAN no late drops" test "$(stat_n "$logf" late_dropped)" -eq 0
    check "VLAN no agg overflow" test "$(stat_n "$logf" agg_overflow)" -eq 0
    check "VLAN bpftool shows program attached" test -n "$ID_C"
}

# ------------------------------------------------------------------- phase D
# One run, both address families: the CSVs must contain dotted-quad IPv4 rows
# and compressed IPv6 rows side by side.
phase_d() {
    local logf="$V/ipv6-probe-mixed.log"
    log "phase D: IPv4 + IPv6 traffic in one run"
    rm -f "$MIXED_CSV" "$MIXED_AGG"
    "$PROBE" --iface veth0 --out "$MIXED_CSV" --agg-out "$MIXED_AGG" \
        "${PROBE_ARGS_COMMON[@]}" >"$logf" 2>&1 &
    PID_D=$!
    sleep 1
    if ! kill -0 "$PID_D" 2>/dev/null; then
        log "FATAL: mixed probe exited early"
        cat "$logf" | tee -a "$LOG"
        PID_D=""
        FAIL=$((FAIL + 1))
        return 0
    fi

    wait_window_start
    ip netns exec "$NS" ping -6 -c 3 -i 0.1 -W 1 fd00:99::1 >"$V/ipv6-ping-mixed-v6.log" 2>&1
    log "ping -6 mixed rc=$?"
    ip netns exec "$NS" ping -c 3 -i 0.1 -W 1 10.99.0.1 >"$V/ipv6-ping-mixed-v4.log" 2>&1
    log "ping -4 mixed rc=$?"
    wait_until_ms $(( $(now_ms) + CLOSE_WAIT_MS ))

    stop_probe "$PID_D" INT
    PID_D=""
    cat "$logf" | tee -a "$LOG"

    local bad_v6 bad_v4 bad_fe80 mapped
    bad_v6=$(awk -F, 'NR>1 && $7==58 && ($3 !~ /:/ || $4 !~ /:/)' "$MIXED_CSV" | wc -l)
    bad_v4=$(awk -F, 'NR>1 && $7==1 && ($3 !~ /^[0-9]+\.[0-9]+\.[0-9]+\.[0-9]+$/ || $4 !~ /^[0-9]+\.[0-9]+\.[0-9]+\.[0-9]+$/)' "$MIXED_CSV" | wc -l)
    bad_fe80=$(awk -F, 'NR>1 && ($3 ~ /^fe80:/ || $4 ~ /^fe80:/) && ($3 !~ /:/ || $4 !~ /:/)' "$MIXED_CSV" | wc -l)
    mapped=$(grep -c '::ffff:' "$MIXED_CSV" || true)
    log "phase D rows: bad_v6=$bad_v6 bad_v4=$bad_v4 bad_fe80=$bad_fe80 mapped=$mapped"

    check "phase D mixed ping -4 3/3 without loss" \
        grep -q '3 received, 0% packet loss' "$V/ipv6-ping-mixed-v4.log"
    check "phase D mixed ping -6 3/3 without loss" \
        grep -q '3 received, 0% packet loss' "$V/ipv6-ping-mixed-v6.log"
    check "phase D sample CSV header is the 7-column contract" \
        test "$(head -n 1 "$MIXED_CSV")" = "$SAMPLE_HEADER"
    check "phase D agg CSV header is the 16-column contract" \
        test "$(head -n 1 "$MIXED_AGG")" = "$AGG_HEADER"
    check_agg_flow "phase D v4" "$MIXED_AGG" veth0 10.99.0.0/24 10.99.0.1 0 1 1 2 3
    check_agg_flow "phase D v6" "$MIXED_AGG" veth0 fd00:99::/64 fd00:99::1 0 58 1 2 3
    check_sample_flow "phase D v4" "$MIXED_CSV" veth0 10.99.0.2 10.99.0.1 1 2
    check_sample_flow "phase D v6" "$MIXED_CSV" veth0 fd00:99::2 fd00:99::1 58 2
    check "phase D every protocol 58 row has IPv6 addresses ($bad_v6 bad)" test "$bad_v6" -eq 0
    check "phase D every protocol 1 row has dotted-quad addresses ($bad_v4 bad)" test "$bad_v4" -eq 0
    check "phase D fe80:: rows are formatted as IPv6 ($bad_fe80 bad)" test "$bad_fe80" -eq 0
    check "phase D no IPv4-mapped ::ffff: addresses ($mapped)" test "$mapped" -eq 0
    check "phase D no fe80 bytes decoded as IPv4 (254.128.x.x)" \
        test "$(grep -c '254\.128\.' "$MIXED_CSV" || true)" -eq 0
    check "phase D stats line has all tokens" check_stats_tokens "$logf"
    check "phase D no kernel drops" test "$(stat_n "$logf" kernel_dropped)" -eq 0
    check "phase D no malformed records" test "$(stat_n "$logf" malformed)" -eq 0
    check "phase D no late drops" test "$(stat_n "$logf" late_dropped)" -eq 0
}

phase_ab
phase_c
phase_d

# ---------------------------------------------------------------------- cleanup
log "explicit cleanup before final checks"
cleanup

FINAL_XDP=$(awk '/^xdp:/{in_xdp=1;next} /^[a-z]/{in_xdp=0} in_xdp && NF {print}' "$V/ipv6-bpftool-net-final.log")
check "no XDP attachment left after cleanup" test -z "$FINAL_XDP"
check "no probe process left for this script's CSVs" \
    test -z "$(pgrep -f -- "--agg-out $V/ipv6-" 2>/dev/null || true)"
check "netns $NS removed" test -z "$(ip netns list 2>/dev/null | awk -v ns="$NS" '$1==ns')"
check "veth0 removed" test -z "$(ip link show veth0 2>/dev/null || true)"

log "=== summary ==="
log "phase AB: $V/ipv6-probe-veth0.log, $V/ipv6-probe-veth1.log, $AGG, $CSV"
log "phase C: $V/ipv6-probe-vlan.log, $VLAN_AGG"
log "phase D: $V/ipv6-probe-mixed.log, $MIXED_AGG"
log "final bpftool net list:"
cat "$V/ipv6-bpftool-net-final.log" | tee -a "$LOG"

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
