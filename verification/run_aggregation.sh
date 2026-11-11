#!/usr/bin/env bash
# Acceptance checks A2-A7 for the windowed sampler + userspace aggregator.
#
# Every phase runs the real probe on a private veth pair (agg0 <-> agg1 in
# netns agg-ns) with --window-ms 4000, then drives ICMP/UDP traffic and asserts
# the aggregation CSV (16 columns) and sample CSV (7 columns):
#
#   A2  three ICMP echo requests -> one agg row, count=3, first/last inside the
#       window, window_complete=1, exactly one sample row.
#   A3  two source addresses in the same /24 (10.99.0.2 and 10.99.0.3) -> one
#       agg row with src_prefix=10.99.0.0/24 and count=6, one sample row
#       (sample key uses the masked prefix).
#   A4  `ping -f -c 1000` -> sigma(count)=1000, one sample row per key/window.
#   A5  UDP generator >100k pps for 5 s -> sigma(count) within 1% of sent,
#       sample rows <= keys x windows, agg_overflow does not grow.
#   A6  traffic in two adjacent windows -> two rows, sigma(count)=total, window
#       boundaries adjacent, no duplicate window, late_dropped=0.
#   A7  SIGINT and SIGTERM while a window is open -> partial rows
#       (window_complete=0) are written and the stats line has every token.
#
# Timing: the kernel window id is derived from the monotonic clock while the
# window bounds are wall-clock milliseconds.  Before each measured burst the
# script waits for the wall clock to be just past a (modulo) window boundary so
# three echo requests land inside one window; after the burst it waits
# window+grace+scan before signaling, so the window is emitted closed.
#
# Names are private on purpose (agg-ns / agg0 / agg1) so this script never
# collides with run_e2e*.sh (aya-ns / veth*) or the perf baseline (perf-ns).
# The addresses 10.99.0.0/24 are required by A3; do not run this script at the
# same time as run_e2e.sh (it also puts 10.99.0.1/24 on its own veth).
#
# Logs/CSVs: /workspace/verification/agg-*. Everything is cleaned up on exit.
set -u

V=/workspace/verification
PROBE_DIR=/workspace/probe
PROBE="$PROBE_DIR/target/release/probe"
GEN="${TRAFFIC_GEN_BIN:-/tmp/traffic_gen}"
GEN_SRC="$V/traffic_gen.rs"
NS=agg-ns
A0=agg0
A1=agg1
HOST_IP=10.99.0.1
NS_IP=10.99.0.2
NS_IP2=10.99.0.3
LOG="$V/agg-e2e.log"
BUILD_LOG="$V/agg-build.log"
CLI_HELP="$V/agg-cli-help.log"

WINDOW_MS=4000
GRACE_MS=500
SCAN_MS=200
CLOSE_WAIT_MS=$((WINDOW_MS + GRACE_MS + SCAN_MS + 400))
PROBE_ARGS_COMMON=(--window-ms "$WINDOW_MS" --grace-ms "$GRACE_MS" --scan-ms "$SCAN_MS"
    --src-prefix-v4 24 --src-prefix-v6 64 --flush-ms 100)

SAMPLE_HEADER='ts_unix_ms,iface,src_ip,dst_ip,src_port,dst_port,protocol'
AGG_HEADER='window_start_ms,window_end_ms,iface,direction,src_prefix,dst_addr,dst_port,protocol,action,rule_id,count,first_seen_ms,last_seen_ms,sample_dropped,degraded,window_complete'
STATS_TOKENS='packets_total sample_rows events_received rows_written agg_records kernel_dropped sample_failed agg_overflow malformed late_dropped samples_deduped'

FAIL=0
SKIP=0
CLEANED=0
SPAWNED_PID=""
PROBE_PIDS=()
RECV_PIDS=()

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

skip() {
    log "CHECK SKIP: $*"
    SKIP=$((SKIP + 1))
}

# --------------------------------------------------------------- small helpers
now_ms() { date +%s%3N; }

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

# Offset of the kernel's window boundaries on the wall clock, in milliseconds:
# window boundaries are at wall-clock instants `n * WINDOW_MS + WIN_PHASE`.
# It is NOT 0: the userspace WallClock offset and the monotonic ktime make the
# kernel window start land at an arbitrary phase (measured here: ~2796..2800 ms
# for WINDOW_MS=4000).  The one-off calibration run below measures it from a
# real `window_start_ms`, then every later phase waits for the *true* boundary.
WIN_PHASE=""

# Milliseconds since the last kernel window boundary (0..WINDOW_MS-1).
win_pos() {
    local p=$(( ( $(now_ms) - ${WIN_PHASE:-0} ) % WINDOW_MS ))
    [ "$p" -lt 0 ] && p=$((p + WINDOW_MS))
    printf '%s' "$p"
}

# Wait until the wall clock is just past a kernel window boundary.
# With WIN_PHASE set (after calibration) this aligns with the data path; without
# it, fall back to the plain wall-clock modulo wait.
wait_window_start() {
    local deadline=$(( $(now_ms) + 20000 ))
    if [ -n "${WIN_PHASE:-}" ]; then
        while [ "$(win_pos)" -ge 400 ]; do
            if [ "$(now_ms)" -ge "$deadline" ]; then
                log "WARN: window-start alignment wait timed out (phase=$WIN_PHASE)"
                return 1
            fi
            sleep 0.05
        done
        return 0
    fi
    while [ $(( $(now_ms) % WINDOW_MS )) -ge 500 ]; do
        if [ "$(now_ms)" -ge "$deadline" ]; then
            log "WARN: window-start alignment wait timed out"
            return 1
        fi
        sleep 0.05
    done
    return 0
}

# Wait for the NEXT kernel window boundary (used by A6 to put one burst in
# window W and the next burst in window W+1).
wait_next_window_start() {
    local deadline=$(( $(now_ms) + 20000 ))
    if [ -n "${WIN_PHASE:-}" ]; then
        # Step away from the boundary we are sitting on first, otherwise
        # wait_window_start would return immediately in the same window.
        while [ "$(win_pos)" -lt 1000 ]; do
            if [ "$(now_ms)" -ge "$deadline" ]; then
                log "WARN: next-window wait timed out"
                return 1
            fi
            sleep 0.05
        done
    else
        sleep 1
    fi
    wait_window_start
}

wait_until_ms() {
    local target=$1 now
    while :; do
        now=$(now_ms)
        [ "$now" -ge "$target" ] && break
        if [ $((target - now)) -gt 200 ]; then sleep 0.1; else sleep 0.02; fi
    done
}

# --------------------------------------------------------------- probe helpers
spawn_probe() {
    # iface out agg logf [extra probe args]
    local iface=$1 out=$2 agg=$3 logf=$4
    shift 4
    "$PROBE" --iface "$iface" --out "$out" --agg-out "$agg" \
        "${PROBE_ARGS_COMMON[@]}" "$@" >"$logf" 2>&1 &
    SPAWNED_PID=$!
    PROBE_PIDS+=("$SPAWNED_PID")
}

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

# Wait until the freshly spawned probe still runs, then announce its CSV paths.
probe_alive() {
    local pid=$1 logf=$2
    sleep 1
    if ! kill -0 "$pid" 2>/dev/null; then
        log "FATAL: probe exited early"
        cat "$logf" | tee -a "$LOG"
        return 1
    fi
    return 0
}

# Kill only probes this script owns (never another script's probe): match the
# --agg-out paths that only this run uses.  Matching on "--agg-out" keeps
# `tee -a <log>` and other helper processes out of the pattern.
kill_stale_probes() {
    pkill -f -- "--agg-out $V/agg-" 2>/dev/null || true
    pkill -f -- "--agg-out $V/traffic" 2>/dev/null || true
}

cleanup() {
    [ "$CLEANED" -eq 1 ] && return 0
    CLEANED=1
    log "cleanup: stopping probes, generator, veth and netns"
    local pid
    for pid in "${RECV_PIDS[@]:-}"; do [ -n "$pid" ] && stop_probe "$pid" TERM; done
    for pid in "${PROBE_PIDS[@]:-}"; do [ -n "$pid" ] && stop_probe "$pid" INT; done
    kill_stale_probes

    ip link del "$A0" 2>/dev/null || true
    ip netns del "$NS" 2>/dev/null || true

    sleep 0.3
    bpftool net list >"$V/agg-bpftool-net-final.log" 2>&1
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
    awk -F, -v iface="$2" -v src="$3" -v dst="$4" -v proto="$5" -v dport="$6" '
        NR > 1 {
            if (iface != "" && $2 != iface) next
            if (src != "" && $3 != src) next
            if (dst != "" && $4 != dst) next
            if (proto != "" && $7 + 0 != proto + 0) next
            if (dport != "" && $6 + 0 != dport + 0) next
            n++
        }
        END { print n + 0 }' "$1"
}

check_sample_flow() {
    # desc file iface src dst proto dport maxrows
    local desc=$1 file=$2 iface=$3 src=$4 dst=$5 proto=$6 dport=$7 maxrows=$8
    local n
    n=$(sample_count "$file" "$iface" "$src" "$dst" "$proto" "$dport")
    log "sample($desc): rows=$n (iface=$iface ${src:-*} -> $dst protocol=$proto dport=${dport:-*})"
    check "$desc: sample has 1..$maxrows row(s) for one key/window (got $n)" \
        test "$n" -ge 1 -a "$n" -le "$maxrows"
}

# Common post-phase checks: token contract + clean counters.
check_probe_health() {
    # desc logf
    local desc=$1 logf=$2
    check "$desc: stats line has all tokens" check_stats_tokens "$logf"
    check "$desc: no kernel drops" test "$(stat_n "$logf" kernel_dropped)" -eq 0
    check "$desc: no malformed records" test "$(stat_n "$logf" malformed)" -eq 0
    check "$desc: no late drops" test "$(stat_n "$logf" late_dropped)" -eq 0
    check "$desc: no agg overflow" test "$(stat_n "$logf" agg_overflow)" -eq 0
}

check_headers() {
    # desc agg sample
    local desc=$1 agg=$2 sample=$3
    check "$desc: sample CSV header is the 7-column contract" \
        test "$(head -n 1 "$sample")" = "$SAMPLE_HEADER"
    check "$desc: agg CSV header is the 16-column contract" \
        test "$(head -n 1 "$agg")" = "$AGG_HEADER"
    check "$desc: sample CSV has exactly one header" \
        test "$(grep -c '^ts_unix_ms,iface' "$sample")" -eq 1
    check "$desc: agg CSV has exactly one header" \
        test "$(grep -c '^window_start_ms,window_end_ms' "$agg")" -eq 1
}

show_agg_rows() {
    # desc agg [max]
    local desc=$1 agg=$2 max=${3:-4}
    log "agg($desc) rows:"
    head -n $((max + 1)) "$agg" | tee -a "$LOG"
}

# ======================================================================= start
: >"$LOG"
log "=== aggregation acceptance A2-A7 start ==="

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
ip link del "$A0" 2>/dev/null || true
ip netns del "$NS" 2>/dev/null || true

sysctl -qw kernel.bpf_stats_enabled=1 2>>"$LOG" || true
sysctl -qw net.ipv4.icmp_echo_ignore_broadcasts=0 2>>"$LOG" || true

log "creating netns $NS and veth pair $A0 <-> $A1"
ip netns add "$NS" || { log "FATAL: ip netns add failed"; exit 1; }
ip link add "$A0" type veth peer name "$A1" || { log "FATAL: veth add failed"; exit 1; }
ip link set "$A1" netns "$NS" || exit 1
# IPv4-only scenario: keep ND/MLD noise out of the CSVs.
echo 1 >/proc/sys/net/ipv6/conf/"$A0"/disable_ipv6 2>>"$LOG" || true
ip netns exec "$NS" sh -c "echo 1 >/proc/sys/net/ipv6/conf/$A1/disable_ipv6" 2>>"$LOG" || true

ip addr add "$HOST_IP/24" dev "$A0" || exit 1
ip link set "$A0" up || exit 1
ip netns exec "$NS" ip link set lo up || exit 1
ip netns exec "$NS" ip addr add "$NS_IP/24" dev "$A1" || exit 1
ip netns exec "$NS" ip link set "$A1" up || exit 1

ip -br addr show "$A0" | tee -a "$LOG"
ip netns exec "$NS" ip -br addr show "$A1" | tee -a "$LOG"

# Warm ARP before any probe attaches: the measured bursts must not be delayed
# by neighbour discovery.
ip netns exec "$NS" ping -c 1 -W 1 "$HOST_IP" >"$V/agg-preheat.log" 2>&1 || true

# ---------------------------------------------------------------- calibration
# One throwaway probe/ping run measures the *real* window phase: the kernel
# window id comes from ktime, so after the userspace WallClock conversion the
# emitted window bounds are at `n * WINDOW_MS + phase`, and that phase is NOT 0
# (measured ~2780 ms for WINDOW_MS=4000).  Every later phase waits for a true
# boundary using it, which is what makes "exactly one window row" (A2) and
# "two adjacent windows" (A6) deterministic.
#
# The phase is read from window_end_ms, which is always an exact boundary.
# The earlier first-window clamp in userspace (Clock::unix_ms used
# saturating_sub for timestamps sampled before the clock) is FIXED: unix_ms now
# supports a negative delta, so window_start_ms is a real boundary even for the
# first window that began before the process sampled its clock, and
# window_end_ms - window_start_ms == WINDOW_MS holds for every closed window.
calibrate_window_phase() {
    local agg="$V/agg-calib.csv" sample="$V/agg-calib-samples.csv" logf="$V/agg-calib-probe.log"
    log "window phase: calibrating kernel window boundary offset (one throwaway ping)"
    rm -f "$agg" "$sample"
    spawn_probe "$A0" "$sample" "$agg" "$logf"
    local pid=$SPAWNED_PID
    if ! probe_alive "$pid" "$logf"; then
        FAIL=$((FAIL + 1))
        return 1
    fi
    wait_window_start
    ip netns exec "$NS" ping -c 1 -W 1 "$HOST_IP" >"$V/agg-calib-ping.log" 2>&1 || true
    wait_until_ms $(( $(now_ms) + CLOSE_WAIT_MS ))
    stop_probe "$pid" INT
    local we
    we=$(awk -F, 'NR==2{print $2}' "$agg" 2>/dev/null | head -1)
    if [ -z "${we:-}" ]; then
        log "WARN: window phase calibration produced no agg row; falling back to wall-clock modulo wait"
        return 1
    fi
    WIN_PHASE=$(( we % WINDOW_MS ))
    log "window phase: first window_end_ms=$we -> WIN_PHASE=${WIN_PHASE}ms (kernel boundaries at wall-clock n*${WINDOW_MS}+${WIN_PHASE}, NOT at n*${WINDOW_MS})"
    return 0
}

calibrate_window_phase || true

# ------------------------------------------------------------------------ A2
# Three ICMP echo requests -> one aggregation row, count=3, one sample row.
phase_a2() {
    local agg="$V/agg-a2.csv" sample="$V/agg-a2-samples.csv" logf="$V/agg-a2-probe.log"
    log "A2: 3 ICMP echo requests -> one agg row count=3, one sample row"
    rm -f "$agg" "$sample"
    spawn_probe "$A0" "$sample" "$agg" "$logf"
    local pid=$SPAWNED_PID
    probe_alive "$pid" "$logf" || { FAIL=$((FAIL + 1)); return 0; }

    wait_window_start
    ip netns exec "$NS" ping -c 3 -i 0.2 -W 1 "$HOST_IP" >"$V/agg-a2-ping.log" 2>&1
    log "A2 ping rc=$?"
    cat "$V/agg-a2-ping.log" | tee -a "$LOG"
    wait_until_ms $(( $(now_ms) + CLOSE_WAIT_MS ))
    stop_probe "$pid" INT
    cat "$logf" | tee -a "$LOG"

    check "A2 ping 3/3 without loss" grep -q '3 received, 0% packet loss' "$V/agg-a2-ping.log"
    check_headers "A2" "$agg" "$sample"
    check_agg_flow "A2" "$agg" "$A0" 10.99.0.0/24 "$HOST_IP" 0 1 1 1 3
    # The Clock::unix_ms negative-delta fix makes the first window start a real
    # boundary, so a closed window must always span exactly WINDOW_MS.
    A2_SPAN=$(awk -F, 'NR==2{print $2 - $1}' "$agg" 2>/dev/null | head -1)
    check "A2 window span is WINDOW_MS (got ${A2_SPAN:-0}, want $WINDOW_MS)" \
        test "${A2_SPAN:-0}" -eq "$WINDOW_MS"
    check_sample_flow "A2" "$sample" "$A0" "$NS_IP" "$HOST_IP" 1 "" 1
    check_probe_health "A2" "$logf"
    check "A2 stat sample_rows=1 (got $(stat_n "$logf" sample_rows))" \
        test "$(stat_n "$logf" sample_rows)" -eq 1
    check "A2 stat agg_records=1 (got $(stat_n "$logf" agg_records))" \
        test "$(stat_n "$logf" agg_records)" -eq 1
    show_agg_rows "A2" "$agg"
}

# ------------------------------------------------------------------------ A3
# Two source addresses in 10.99.0.0/24 -> one row, count=6, one sample row.
phase_a3() {
    local agg="$V/agg-a3.csv" sample="$V/agg-a3-samples.csv" logf="$V/agg-a3-probe.log"
    log "A3: sources $NS_IP and $NS_IP2 (same /24) -> one agg row, src_prefix=10.99.0.0/24, count=6"
    ip netns exec "$NS" ip addr add "$NS_IP2/24" dev "$A1" 2>>"$LOG" || true
    ip netns exec "$NS" ping -c 1 -W 1 -I "$NS_IP2" "$HOST_IP" >"$V/agg-a3-preheat2.log" 2>&1 || true

    rm -f "$agg" "$sample"
    spawn_probe "$A0" "$sample" "$agg" "$logf"
    local pid=$SPAWNED_PID
    probe_alive "$pid" "$logf" || { FAIL=$((FAIL + 1)); return 0; }

    wait_window_start
    ip netns exec "$NS" ping -c 3 -i 0.2 -W 1 -I "$NS_IP" "$HOST_IP" >"$V/agg-a3-ping-1.log" 2>&1
    log "A3 ping from $NS_IP rc=$?"
    ip netns exec "$NS" ping -c 3 -i 0.2 -W 1 -I "$NS_IP2" "$HOST_IP" >"$V/agg-a3-ping-2.log" 2>&1
    log "A3 ping from $NS_IP2 rc=$?"
    wait_until_ms $(( $(now_ms) + CLOSE_WAIT_MS ))
    stop_probe "$pid" INT
    cat "$logf" | tee -a "$LOG"

    check "A3 ping from $NS_IP 3/3 without loss" grep -q '3 received, 0% packet loss' "$V/agg-a3-ping-1.log"
    check "A3 ping from $NS_IP2 3/3 without loss" grep -q '3 received, 0% packet loss' "$V/agg-a3-ping-2.log"
    check_headers "A3" "$agg" "$sample"
    # src_prefix must be the masked /24, not a /32 or the full address.
    check_agg_flow "A3" "$agg" "$A0" 10.99.0.0/24 "$HOST_IP" 0 1 1 1 6
    check_sample_flow "A3" "$sample" "$A0" "" "$HOST_IP" 1 "" 1
    check "A3 sample src is one of the two source addresses" \
        test "$(awk -F, -v a="$NS_IP" -v b="$NS_IP2" 'NR>1 && ($3==a || $3==b) {n++} END{print n+0}' "$sample")" -eq 1
    check_probe_health "A3" "$logf"
    check "A3 stat sample_rows=1 (got $(stat_n "$logf" sample_rows))" \
        test "$(stat_n "$logf" sample_rows)" -eq 1
    show_agg_rows "A3" "$agg"
}

# ------------------------------------------------------------------------ A4
# Flood: ping -f -c 1000.  Sigma(count) must be exactly 1000 even though the
# sample CSV only holds one row for the key/window.
phase_a4() {
    local agg="$V/agg-a4.csv" sample="$V/agg-a4-samples.csv" logf="$V/agg-a4-probe.log"
    log "A4: ping -f -c 1000 -> sigma(count)=1000, one sample row per key/window"
    rm -f "$agg" "$sample"
    spawn_probe "$A0" "$sample" "$agg" "$logf"
    local pid=$SPAWNED_PID
    probe_alive "$pid" "$logf" || { FAIL=$((FAIL + 1)); return 0; }

    wait_window_start
    ip netns exec "$NS" ping -f -c 1000 -W 2 "$HOST_IP" >"$V/agg-a4-ping.log" 2>&1
    log "A4 ping -f rc=$?"
    tail -n 3 "$V/agg-a4-ping.log" | tee -a "$LOG"
    wait_until_ms $(( $(now_ms) + CLOSE_WAIT_MS ))
    stop_probe "$pid" INT
    cat "$logf" | tee -a "$LOG"

    check "A4 flood ping 1000/1000 without loss" grep -q '1000 received, 0% packet loss' "$V/agg-a4-ping.log"
    check_headers "A4" "$agg" "$sample"
    check_agg_flow "A4" "$agg" "$A0" 10.99.0.0/24 "$HOST_IP" 0 1 1 2 1000
    check_sample_flow "A4" "$sample" "$A0" "$NS_IP" "$HOST_IP" 1 "" 2
    check_probe_health "A4" "$logf"
    check "A4 stat packets_total >= 1000 (got $(stat_n "$logf" packets_total))" \
        test "$(stat_n "$logf" packets_total)" -ge 1000
    show_agg_rows "A4" "$agg"
}

# ------------------------------------------------------------------------ A5
# High-rate UDP (perf's generator) -> aggregation is loss-accurate and the
# accounting counters stay sane.  Preferred mode: the generator's `--dports N`
# spreads one packet stream over N destination ports (= N aggregation keys),
# which is what can overflow the 256 KiB sample ring; the fallback mode runs a
# few generator processes on distinct ports.  SKIP (not FAIL) when the
# generator is absent.
phase_a5() {
    local agg="$V/agg-a5.csv" sample="$V/agg-a5-samples.csv" logf="$V/agg-a5-probe.log"
    local -a ports=(9990 9991 9992 9993)
    local pps_per=40000 secs=5
    log "A5: high-rate UDP aggregation for ${secs}s (mode chosen from generator capabilities) -> sigma(count) within 1% of sent"

    if [ ! -x "$GEN" ] && [ -f "$GEN_SRC" ]; then
        log "building traffic generator: rustc -O $GEN_SRC -o $GEN"
        rustc -O "$GEN_SRC" -o "$GEN" >"$V/traffic_gen-build.log" 2>&1 || true
    fi
    if [ ! -x "$GEN" ]; then
        skip "A5 high-rate aggregation: traffic generator $GEN (source $GEN_SRC) not available; TODO: wire perf's verification/traffic_gen.rs; A5 NOT executed"
        return 0
    fi

    rm -f "$agg" "$sample"
    spawn_probe "$A0" "$sample" "$agg" "$logf"
    local pid=$SPAWNED_PID
    probe_alive "$pid" "$logf" || { FAIL=$((FAIL + 1)); return 0; }

    # Preferred mode: a generator with --dports emits a fresh destination port
    # per packet, i.e. tens of thousands of aggregation keys, which is what can
    # actually overflow the 256 KiB sample ring.  Without --dports we fall back
    # to a handful of generator processes on distinct ports.
    local dport_keys=0
    if "$GEN" --help 2>&1 | grep -q -- '--dports'; then
        dport_keys=50000
        log "A5: many-key mode: generator supports --dports -> 100k pps over $dport_keys destination ports"
    else
        log "A5: fallback mode: generator has no --dports; ${#ports[@]} processes x ${pps_per}pps on ${#ports[@]} UDP ports"
    fi

    local total_sent=0 total_pps=0 total_err=0 s p e sendline port rpid spid
    if [ "$dport_keys" -gt 0 ]; then
        # No receiver is needed: ICMP port-unreachable replies travel
        # host->namespace, never reach agg0 ingress, and are excluded by the
        # protocol=17 filter anyway.
        wait_window_start
        ip netns exec "$NS" "$GEN" send --dst "$HOST_IP:9990" \
            --pps "$((pps_per * ${#ports[@]}))" --secs "$secs" \
            --dports "$dport_keys" --payload 64 >"$V/agg-a5-send.log" 2>&1
        sendline=$(tail -n 1 "$V/agg-a5-send.log")
        log "A5 send: ${sendline:-<no output>}"
        total_sent=$(sed -n 's/.*sent=\([0-9][0-9]*\).*/\1/p' "$V/agg-a5-send.log" | tail -1); total_sent=${total_sent:-0}
        total_pps=$(sed -n 's/.* pps=\([0-9][0-9]*\).*/\1/p' "$V/agg-a5-send.log" | tail -1); total_pps=${total_pps:-0}
        total_err=$(sed -n 's/.*send_errors=\([0-9][0-9]*\).*/\1/p' "$V/agg-a5-send.log" | tail -1); total_err=${total_err:-0}
    else
        # One receiver per destination port keeps the kernel from answering the
        # UDP flood with ICMP port-unreachable and lets us confirm the target.
        for port in "${ports[@]}"; do
            "$GEN" recv --bind "$HOST_IP:$port" --secs $((secs + 5)) >"$V/agg-a5-recv-$port.log" 2>&1 &
            rpid=$!
            RECV_PIDS+=("$rpid")
        done
        sleep 0.3

        wait_window_start
        local -a spids=()
        for port in "${ports[@]}"; do
            ip netns exec "$NS" "$GEN" send --dst "$HOST_IP:$port" --pps "$pps_per" \
                --secs "$secs" --payload 64 >"$V/agg-a5-send-$port.log" 2>&1 &
            spids+=($!)
        done
        for spid in "${spids[@]}"; do
            wait "$spid" 2>/dev/null || true
        done
        for port in "${ports[@]}"; do
            s=$(sed -n 's/.*sent=\([0-9][0-9]*\).*/\1/p' "$V/agg-a5-send-$port.log" | tail -1)
            p=$(sed -n 's/.* pps=\([0-9][0-9]*\).*/\1/p' "$V/agg-a5-send-$port.log" | tail -1)
            e=$(sed -n 's/.*send_errors=\([0-9][0-9]*\).*/\1/p' "$V/agg-a5-send-$port.log" | tail -1)
            sendline=$(tail -n 1 "$V/agg-a5-send-$port.log" 2>/dev/null)
            log "A5 port $port: ${sendline:-<no output>}"
            total_sent=$((total_sent + ${s:-0}))
            total_pps=$((total_pps + ${p:-0}))
            total_err=$((total_err + ${e:-0}))
        done
    fi
    log "A5 totals: sent=$total_sent pps=$total_pps send_errors=$total_err"

    wait_until_ms $(( $(now_ms) + CLOSE_WAIT_MS ))
    stop_probe "$pid" INT
    for rpid in "${RECV_PIDS[@]:-}"; do [ -n "$rpid" ] && stop_probe "$rpid" TERM; done
    cat "$logf" | tee -a "$LOG"
    for port in "${ports[@]}"; do
        [ -f "$V/agg-a5-recv-$port.log" ] && log "A5 recv[$port]: $(tail -n 1 "$V/agg-a5-recv-$port.log")"
    done

    # Row budget: one row per key per window; keys = dports (or ports), windows<=3.
    local max_rows
    if [ "$dport_keys" -gt 0 ]; then max_rows=$((dport_keys * 3)); else max_rows=$((${#ports[@]} * 3)); fi

    check "A5 generator sent >100k pps for 5s (sent=$total_sent pps=$total_pps)" \
        test "$total_sent" -ge 500000 -a "$total_pps" -ge 100000
    check "A5 generator send_errors=0 (got $total_err)" test "$total_err" -eq 0
    check_headers "A5" "$agg" "$sample"

    # Filter by prefix/dst/UDP; all generator ports/keys are included.
    local out n sum cbad tbad dbad abad rest
    out=$(agg_rows "$agg" "$A0" 10.99.0.0/24 "$HOST_IP" "" 17 -1)
    read -r n sum cbad tbad dbad abad rest <<<"$out"
    log "agg(A5): rows=${n:-?} sum=${sum:-?} complete_bad=${cbad:-?} time_bad=${tbad:-?} dir_bad=${dbad:-?} action_bad=${abad:-?} ${rest:-} sent=$total_sent"
    check "A5 agg sigma(count) within 1% of sent ($sum vs $total_sent)" \
        awk -v s="$sum" -v e="$total_sent" 'BEGIN { d = s - e; if (d < 0) d = -d; exit !(d <= e * 0.01) }'
    check "A5 agg rows 1..$max_rows (got ${n:-0})" \
        test "${n:-0}" -ge 1 -a "${n:-0}" -le "$max_rows"
    check "A5 agg direction/action ok (bad ${dbad:-1}/${abad:-1})" \
        test "${dbad:-1}" -eq 0 -a "${abad:-1}" -eq 0
    check "A5 agg window_complete is 1 for closed windows (bad ${cbad:-?})" test "${cbad:-1}" -eq 0
    check "A5 agg first/last inside window (bad ${tbad:-?})" test "${tbad:-1}" -eq 0

    # Sample volume: at most one row per key per window.
    local sn
    sn=$(sample_count "$sample" "$A0" "" "$HOST_IP" 17 "")
    local max_samples
    if [ "$dport_keys" -gt 0 ]; then max_samples=$((dport_keys * 3)); else max_samples=$((${#ports[@]} * 3)); fi
    check "A5 sample rows <= keys x windows = $max_samples (got $sn)" test "$sn" -le "$max_samples"
    check "A5 stat packets_total >= sent (got $(stat_n "$logf" packets_total) vs $total_sent)" \
        test "$(stat_n "$logf" packets_total)" -ge "$total_sent"
    check "A5 stat agg_overflow did not grow (got $(stat_n "$logf" agg_overflow))" \
        test "$(stat_n "$logf" agg_overflow)" -eq 0
    check "A5 stats line has all tokens" check_stats_tokens "$logf"
    check "A5 no malformed records" test "$(stat_n "$logf" malformed)" -eq 0
    check "A5 no late drops" test "$(stat_n "$logf" late_dropped)" -eq 0

    # Sample loss accounting: with many distinct keys the sample ring must
    # overflow, so sample_failed / sample_dropped must be non-zero and the
    # aggregates must still be exact (asserted above).  Without --dports the
    # key count is too small to fill 256 KiB: report it as not executed.
    local sf sd
    sf=$(stat_n "$logf" sample_failed)
    sd=$(awk -F, 'NR>1 {s+=$14} END{print s+0}' "$agg")
    log "A5 sample loss counters: sample_failed=$sf sample_dropped(sum col14)=$sd"
    if [ "$dport_keys" -gt 0 ]; then
        check "A5 sample loss was counted (sample_failed=$sf sample_dropped=$sd)" \
            test "$sf" -gt 0 -o "$sd" -gt 0
    elif [ "$sf" -gt 0 ] || [ "$sd" -gt 0 ]; then
        log "CHECK PASS: A5 sample loss counters are non-zero (sample_failed=$sf sample_dropped=$sd)"
    else
        skip "A5 sample_failed/sample_dropped > 0: needs generator --dports (many keys); current generator has none, so the 256 KiB sample ring cannot overflow; A5 count fidelity asserted instead"
    fi
    show_agg_rows "A5" "$agg" 6
}

# ------------------------------------------------------------------------ A6
# Traffic in two adjacent windows -> two rows whose windows touch, sigma=total.
# The kernel window phase is calibrated first, so burst 1 starts just after a
# real window boundary W and burst 2 just after W+1 (a wall-clock modulo wait
# would land in W, W+2 because the boundaries are ktime-offset, not at n*4000).
phase_a6() {
    local agg="$V/agg-a6.csv" sample="$V/agg-a6-samples.csv" logf="$V/agg-a6-probe.log"
    log "A6: two adjacent windows -> two rows, sigma(count)=6, no duplicates"
    rm -f "$agg" "$sample"
    spawn_probe "$A0" "$sample" "$agg" "$logf"
    local pid=$SPAWNED_PID
    probe_alive "$pid" "$logf" || { FAIL=$((FAIL + 1)); return 0; }

    # Window W.  Start from a guaranteed-future boundary (wait_next_window_start
    # steps away from the boundary we might be sitting on), so W starts after
    # the probe sampled its Clock and its window_start_ms is exact.
    wait_next_window_start
    ip netns exec "$NS" ping -c 3 -i 0.05 -W 1 "$HOST_IP" >"$V/agg-a6-ping-1.log" 2>&1
    log "A6 window W ping rc=$?"
    # Window W+1.
    wait_next_window_start
    ip netns exec "$NS" ping -c 3 -i 0.05 -W 1 "$HOST_IP" >"$V/agg-a6-ping-2.log" 2>&1
    log "A6 window W+1 ping rc=$?"
    wait_until_ms $(( $(now_ms) + CLOSE_WAIT_MS ))
    stop_probe "$pid" INT
    cat "$logf" | tee -a "$LOG"

    check "A6 window W ping 3/3 without loss" grep -q '3 received, 0% packet loss' "$V/agg-a6-ping-1.log"
    check "A6 window W+1 ping 3/3 without loss" grep -q '3 received, 0% packet loss' "$V/agg-a6-ping-2.log"
    check_headers "A6" "$agg" "$sample"

    # Collect the matching rows sorted by window_start: "start end count complete".
    local -a rows=()
    mapfile -t rows < <(awk -F, -v iface="$A0" -v dst="$HOST_IP" '
        NR > 1 && $3 == iface && $6 == dst && $7 == 0 && $8 == 1 { print $1, $2, $11, $16 }' "$agg" | sort -n)
    local n=${#rows[@]} sum=0 idx complete_bad=0
    for ((idx = 0; idx < n; idx++)); do
        sum=$((sum + $(awk '{print $3}' <<<"${rows[$idx]}")))
        complete_bad=$((complete_bad + $(awk '{print ($4 == 1) ? 0 : 1}' <<<"${rows[$idx]}")))
    done
    log "A6 rows=$n sum=$sum rows_detail=${rows[*]:-<none>}"

    check "A6 exactly two window rows (got $n)" test "$n" -eq 2
    check "A6 sigma(count)=6 (got $sum)" test "$sum" -eq 6
    check "A6 all rows window_complete=1 (bad $complete_bad)" test "$complete_bad" -eq 0
    if [ "$n" -eq 2 ]; then
        local ws1 we1 ws2 we2
        read -r ws1 we1 ignore1 ignore2 <<<"${rows[0]}"
        read -r ws2 we2 ignore1 ignore2 <<<"${rows[1]}"
        log "A6 windows: [$ws1,$we1] [$ws2,$we2]"
        check "A6 first window spans WINDOW_MS ($ws1..$we1)" test $((we1 - ws1)) -eq "$WINDOW_MS"
        check "A6 second window spans WINDOW_MS ($ws2..$we2)" test $((we2 - ws2)) -eq "$WINDOW_MS"
        check "A6 windows are adjacent ($we1 == $ws2)" test "$we1" -eq "$ws2"
        check "A6 no duplicate window_start ($ws1 != $ws2)" test "$ws1" -ne "$ws2"
        check "A6 second window starts one window after the first" \
            test $((ws2 - ws1)) -eq "$WINDOW_MS"
    fi
    # At most one sample per key per window; two windows -> 2 rows, three only
    # if a burst straddled a boundary.  It must never be one row per packet.
    check_sample_flow "A6" "$sample" "$A0" "$NS_IP" "$HOST_IP" 1 "" 3
    check "A6 sample has 2 or 3 rows (got $(sample_count "$sample" "$A0" "$NS_IP" "$HOST_IP" 1 ""))" \
        test "$(sample_count "$sample" "$A0" "$NS_IP" "$HOST_IP" 1 "")" -ge 2
    check_probe_health "A6" "$logf"
    show_agg_rows "A6" "$agg" 5
}

# ------------------------------------------------------------------------ A7
# SIGINT and SIGTERM while a window is still open must flush partial rows
# (window_complete=0) and still print the full stats line.
phase_a7() {
    local sig agg sample logf
    for sig in INT TERM; do
        agg="$V/agg-a7-${sig}.csv"
        sample="$V/agg-a7-${sig}-samples.csv"
        logf="$V/agg-a7-${sig}-probe.log"
        log "A7: SIG$sig while the window is open -> partial row(s) with window_complete=0"
        rm -f "$agg" "$sample"
        spawn_probe "$A0" "$sample" "$agg" "$logf"
        local pid=$SPAWNED_PID
        if ! probe_alive "$pid" "$logf"; then
            FAIL=$((FAIL + 1))
            continue
        fi

        wait_window_start
        # ~20 ms burst, then signal immediately: no scan can have closed the
        # window yet (grace is 500 ms), so the exit path must write partial rows.
        ip netns exec "$NS" ping -c 3 -i 0.01 -W 1 "$HOST_IP" >"$V/agg-a7-${sig}-ping.log" 2>&1
        log "A7 SIG$sig ping rc=$?"
        stop_probe "$pid" "$sig"
        cat "$logf" | tee -a "$LOG"

        check "A7 SIG$sig ping 3/3 without loss" grep -q '3 received, 0% packet loss' "$V/agg-a7-${sig}-ping.log"
        check_headers "A7 SIG$sig" "$agg" "$sample"
        check_agg_flow "A7 SIG$sig" "$agg" "$A0" 10.99.0.0/24 "$HOST_IP" 0 1 0 2 3
        check_sample_flow "A7 SIG$sig" "$sample" "$A0" "$NS_IP" "$HOST_IP" 1 "" 2
        check "A7 SIG$sig stats line has all tokens" check_stats_tokens "$logf"
        check "A7 SIG$sig no late drops" test "$(stat_n "$logf" late_dropped)" -eq 0
        check "A7 SIG$sig no malformed records" test "$(stat_n "$logf" malformed)" -eq 0
        show_agg_rows "A7 SIG$sig" "$agg"
    done
}

phase_a2
phase_a3
phase_a4
phase_a5
phase_a6
phase_a7

# ---------------------------------------------------------------------- cleanup
log "explicit cleanup before final checks"
cleanup

FINAL_XDP=$(awk '/^xdp:/{in_xdp=1;next} /^[a-z]/{in_xdp=0} in_xdp && NF {print}' "$V/agg-bpftool-net-final.log")
check "no XDP attachment left after cleanup" test -z "$FINAL_XDP"
check "no probe process left for agg CSVs" \
    test -z "$(pgrep -f -- "--agg-out $V/agg-" 2>/dev/null || true)"
check "netns $NS removed" test -z "$(ip netns list 2>/dev/null | awk -v ns="$NS" '$1==ns')"
check "veth $A0 removed" test -z "$(ip link show "$A0" 2>/dev/null || true)"

log "=== summary ==="
log "A2: $V/agg-a2.csv, $V/agg-a2-samples.csv"
log "A3: $V/agg-a3.csv, $V/agg-a3-samples.csv"
log "A4: $V/agg-a4.csv, $V/agg-a4-samples.csv"
log "A5: $V/agg-a5.csv, $V/agg-a5-samples.csv (skipped=$([ -s "$V/agg-a5.csv" ] && echo no || echo maybe))"
log "A6: $V/agg-a6.csv, $V/agg-a6-samples.csv"
log "A7: $V/agg-a7-INT.csv, $V/agg-a7-TERM.csv"
log "final bpftool net list:"
cat "$V/agg-bpftool-net-final.log" | tee -a "$LOG"

if [ "$FAIL" -eq 0 ] && [ "$SKIP" -eq 0 ]; then
    log "RESULT: PASS"
elif [ "$FAIL" -eq 0 ]; then
    log "RESULT: PASS ($SKIP check(s) skipped / not executed)"
else
    log "RESULT: FAIL ($FAIL check(s) failed, $SKIP skipped)"
fi
# Not `exit "$FAIL"`: a process status is only 8 bits wide, so a failure count
# of 256 (or any multiple of it) exits 0 and reports a failing suite as green.
[ "$FAIL" -eq 0 ] || exit 1
