#!/usr/bin/env bash
# verification/run_perf.sh — A9: old-vs-new probe performance comparison harness.
#
# Everything runs inside the aya-dev container (the workspace is mounted at /workspace).
# Topology (isolated, never touches aya-ns / veth0 / veth1):
#
#   [netns perf-ns]  perf1 10.99.0.2/24  <-- traffic_gen send (UDP)
#          |
#        veth pair
#          |
#   [host netns]     perf0 10.99.0.1/24  <-- XDP probe attaches here, traffic_gen recv drains it
#
# Usage:
#   bash verification/run_perf.sh              # MODE=old (default)
#   MODE=new bash verification/run_perf.sh     # new per-CPU aggregation implementation
#   MODE=old PPS=100000 DUR=5 bash verification/run_perf.sh
#
# Env knobs:
#   MODE=old|new        which binary to benchmark                     (default old)
#   PPS=100000          target packets per second                     (default 100000)
#   DUR=5               send duration in seconds                      (default 5)
#   PAYLOAD=64          UDP payload bytes 8..1472                     (default 64)
#   PORT=9999           UDP destination port (base port)
#   DPORTS=1            destination port variants: packet i -> base+(i%DPORTS) (default 1;
#                       e.g. DPORTS=50000 spreads the flow over 50000 distinct dst ports,
#                       creating 50000 distinct aggregation keys for the new implementation)
#   RUN_TAG=            artifact name tag, default = MODE (e.g. RUN_TAG=a5 with MODE=new
#                       keeps the A5 capability run separate from the formal perf-new run)
#   PROBE_RUST_LOG=     if set (e.g. info), exported as RUST_LOG for the probe process so
#                       the periodic "metrics: ... scan_ms=..." line is captured in the log
#   FLUSH_MS=500        probe --flush-ms                              (default 500)
#   WINDOW_MS=10000     new impl --window-ms                          (default 10000)
#   GRACE_MS=2000       new impl --grace-ms                           (default 2000)
#   SCAN_MS=1000        new impl --scan-ms                            (default 1000)
#   PROBE_OLD=/tmp/probe-old
#   PROBE_NEW=/workspace/probe/target/release/probe
#   KEEP_NET=1          keep perf-ns/perf0/perf1 after the run        (default: remove)
#   CLEANUP_NET=0       same as KEEP_NET=1 (legacy name)
#
# Outputs:
#   /tmp/perf-<mode>-samples.csv   raw sample CSV written by the probe
#   /tmp/perf-<mode>-agg.csv       aggregation CSV (new impl only)
#   verification/perf/perf-<mode>-{probe,send,recv}.log
#   verification/perf/perf-<mode>-summary.txt   full metric block of this run
#   verification/perf/perf-summary.tsv          one table row per run (accumulated)
#
# CPU% is derived from /proc/<pid>/stat utime+stime (CLK_TCK from getconf), NOT pidstat.
set -uo pipefail

MODE=${MODE:-${1:-old}}
PPS=${PPS:-100000}
DUR=${DUR:-5}
PAYLOAD=${PAYLOAD:-64}
PORT=${PORT:-9999}
DPORTS=${DPORTS:-1}
RUN_TAG=${RUN_TAG:-}
FLUSH_MS=${FLUSH_MS:-500}
WINDOW_MS=${WINDOW_MS:-10000}
GRACE_MS=${GRACE_MS:-2000}
SCAN_MS=${SCAN_MS:-1000}
KEEP_NET=${KEEP_NET:-0}
[ "${CLEANUP_NET:-1}" = "0" ] && KEEP_NET=1

PROBE_OLD=${PROBE_OLD:-/tmp/probe-old}
PROBE_NEW=${PROBE_NEW:-/workspace/probe/target/release/probe}
GEN=${GEN:-/tmp/traffic_gen}
GEN_SRC=${GEN_SRC:-/workspace/verification/traffic_gen.rs}
PERF_DIR=${PERF_DIR:-/workspace/verification/perf}

NS=perf-ns
IFACE=perf0
PEER=perf1
HOST_IP=10.99.0.1
NS_IP=10.99.0.2
DST="$HOST_IP:$PORT"

PROBE_PID=""
RECV_PID=""
SEND_PID=""
CLEANED=0

ts() { date +%Y-%m-%dT%H:%M:%S%z; }
log() { echo "[$(ts)] $*"; }
die() { echo "[$(ts)] FATAL: $*" >&2; exit 1; }

usage() {
    sed -n '2,40p' "$0" | sed 's/^# \{0,1\}//'
    exit 0
}
[ "${1:-}" = "-h" ] || [ "${1:-}" = "--help" ] && usage

# ---------- cleanup ---------------------------------------------------------------
kill_pid() {
    local pid=$1 name=$2 i
    [ -z "$pid" ] && return 0
    kill -0 "$pid" 2>/dev/null || return 0
    kill -INT "$pid" 2>/dev/null
    for i in $(seq 1 100); do
        kill -0 "$pid" 2>/dev/null || return 0
        sleep 0.1
    done
    kill -TERM "$pid" 2>/dev/null
    sleep 0.3
    kill -0 "$pid" 2>/dev/null && kill -KILL "$pid" 2>/dev/null
    wait "$pid" 2>/dev/null
    return 0
}

sigint_and_time() { # pid -> SIGINT, wait, print milliseconds from signal to exit
    local pid=$1 i t0
    [ -z "$pid" ] && { echo 0; return 0; }
    kill -0 "$pid" 2>/dev/null || { echo 0; return 0; }
    t0=$(date +%s%N)
    kill -INT "$pid" 2>/dev/null
    for i in $(seq 1 3000); do # up to 30 s, 10 ms polling
        if ! kill -0 "$pid" 2>/dev/null; then
            echo $(( ($(date +%s%N) - t0) / 1000000 ))
            wait "$pid" 2>/dev/null
            return 0
        fi
        sleep 0.01
    done
    kill -TERM "$pid" 2>/dev/null
    sleep 0.3
    kill -0 "$pid" 2>/dev/null && kill -KILL "$pid" 2>/dev/null
    wait "$pid" 2>/dev/null
    echo $(( ($(date +%s%N) - t0) / 1000000 ))
}

cleanup() {
    [ "$CLEANED" = "1" ] && return 0
    CLEANED=1
    log "cleanup: stopping probe/recv/sender"
    kill_pid "$RECV_PID" recv
    kill_pid "$SEND_PID" send
    kill_pid "$PROBE_PID" probe
    wait 2>/dev/null
    if [ "$KEEP_NET" = "1" ]; then
        log "cleanup: KEEP_NET=1, leaving $NS/$IFACE/$PEER in place"
    else
        log "cleanup: removing $NS and $IFACE/$PEER (only perf-* objects)"
        ip link set "$IFACE" down 2>/dev/null
        ip link del "$IFACE" 2>/dev/null
        ip netns del "$NS" 2>/dev/null
    fi
    log "cleanup done"
}
trap cleanup EXIT INT TERM

# ---------- prerequisites ---------------------------------------------------------
compile_gen() {
    if [ ! -x "$GEN" ] || [ "$GEN_SRC" -nt "$GEN" ]; then
        log "compiling $GEN_SRC -> $GEN"
        rustc -O "$GEN_SRC" -o "$GEN" || die "rustc failed for $GEN_SRC"
    fi
}

ensure_netns() {
    # Remove only our own leftovers, never aya-ns / veth0 / veth1.
    if ip netns list 2>/dev/null | grep -qw "$NS"; then
        log "netns $NS already exists, reusing"
    else
        ip netns add "$NS" || die "cannot create netns $NS"
    fi
    if ip link show "$IFACE" >/dev/null 2>&1; then
        log "link $IFACE already exists, reusing"
    else
        ip link add "$IFACE" type veth peer name "$PEER" || die "cannot create veth $IFACE/$PEER"
        ip link set "$PEER" netns "$NS" || die "cannot move $PEER into $NS"
    fi
    # Idempotent addressing / up.  IPv6 is disabled on both ends of the veth pair:
    # otherwise ND/MLD control traffic (ff02::1, protocol 58/0 rows) reaches the
    # perf0 XDP hook and pollutes "rows written / events" accounting.
    ip link set "$IFACE" down 2>/dev/null
    sysctl -qw "net.ipv6.conf.$IFACE.disable_ipv6=1" 2>/dev/null || true
    ip addr replace "$HOST_IP/24" dev "$IFACE"
    ip link set "$IFACE" up
    ip netns exec "$NS" ip link set "$PEER" down 2>/dev/null
    ip netns exec "$NS" sysctl -qw "net.ipv6.conf.$PEER.disable_ipv6=1" 2>/dev/null || true
    ip netns exec "$NS" sysctl -qw "net.ipv6.conf.lo.disable_ipv6=1" 2>/dev/null || true
    ip netns exec "$NS" ip addr replace "$NS_IP/24" dev "$PEER"
    ip netns exec "$NS" ip link set "$PEER" up
    ip netns exec "$NS" ip link set lo up
    log "ipv6 $IFACE disable_ipv6=$(cat /proc/sys/net/ipv6/conf/$IFACE/disable_ipv6 2>/dev/null || echo n/a) ${PEER}(ns)=$(ip netns exec "$NS" cat /proc/sys/net/ipv6/conf/$PEER/disable_ipv6 2>/dev/null || echo n/a)"
    IPV6_STATE="$IFACE:$(cat /proc/sys/net/ipv6/conf/$IFACE/disable_ipv6 2>/dev/null || echo n/a) $PEER:$(ip netns exec "$NS" cat /proc/sys/net/ipv6/conf/$PEER/disable_ipv6 2>/dev/null || echo n/a)"
    ip route get "$NS_IP" >/dev/null 2>&1 || die "no route from host to $NS_IP via $IFACE"
    if ! ip netns exec "$NS" ping -c 1 -W 1 "$HOST_IP" >/dev/null 2>&1; then
        log "WARN: $NS -> $HOST_IP ping failed (continuing, UDP may still work)"
    fi
}

# ---------- meters ----------------------------------------------------------------
# The probe runs a multi-threaded runtime (probe-old: 21 threads), so process-wide
# CPU must be summed over /proc/<pid>/task/*/stat.  /proc/<pid>/schedstat gives
# nanosecond resolution, which matters because CLK_TCK=100 => one jiffy is 10 ms
# (0.2 % of a 5 s window) and the old implementation's userspace CPU is tiny.
proc_cpu_ns() { # pid -> total CPU nanoseconds across all threads (schedstat field 1)
    local total=0 f v
    for f in /proc/"$1"/task/*/schedstat; do
        [ -r "$f" ] || continue
        read -r v _ <"$f" && total=$((total + v))
    done
    echo "$total"
}
proc_cpu_ticks_all() { # pid -> total utime+stime ticks across all threads
    local t
    t=$(awk '{ sub(/^[^)]*\) /,""); s += $12 + $13 } END { print s + 0 }' \
        /proc/"$1"/task/*/stat 2>/dev/null)
    echo "${t:-0}"
}
proc_threads() { ls "/proc/$1/task" 2>/dev/null | wc -l; }
proc_leader_ticks() { # pid -> "utime stime" of the thread-group leader only
    awk '{ sub(/^[^)]*\) /,""); print $12, $13 }' "/proc/$1/stat" 2>/dev/null
}
procstat_busy() { # sum of busy jiffies across all cpus (idle field 5 excluded)
    awk '/^cpu /{s=0; for (i=2;i<=4;i++) s+=$i; for (i=6;i<=9;i++) s+=$i; print s}' /proc/stat
}
udp_in_errors() { # host UDP receive errors (InErrors + RcvbufErrors), cumulative
    awk '/^Udp:/{if (seen++) {print $4+$6; exit}}' /proc/net/snmp
}
data_rows() { # CSV data rows (header assumed) 
    local f=$1 n
    [ -s "$f" ] || { echo 0; return; }
    n=$(wc -l < "$f")
    [ "$n" -gt 0 ] && echo $((n - 1)) || echo 0
}
agg_sum_count() { # sum of the count-like column of the aggregation CSV
    awk -F, 'NR==1{for(i=1;i<=NF;i++){h=tolower($i); if(h=="count"||h=="pkt_count"||h=="packets"||h=="event_count"||h=="events") c=i} next}
             {s += (c ? $c : $NF)} END{printf "%d", s+0}' "$1"
}
csv_proto_counts() { # protocol=rows breakdown, e.g. "17=51551" (17=UDP, 58=ICMPv6)
    awk -F, 'NR>1{c[$7]++} END{for (k in c) printf "%s=%d ", (k==""?"EMPTY":k), c[k]}' "$1"
}
csv_udp_rows() { awk -F, 'NR>1 && $7==17 {n++} END{print n+0}' "$1"; }
csv_v6_rows() { awk -F, 'NR>1 && /::/ {n++} END{print n+0}' "$1"; }
last_stats_line() {
    grep -E '^stats:|events_received|kernel_dropped' "$1" 2>/dev/null | tail -1
}
field_of() { # field_of "<line>" name -> value or "n/a"
    local line=$1 name=$2 v
    v=$(printf '%s\n' "$line" | sed -n "s/.*\b$name=\([^ ]*\).*/\1/p")
    [ -n "$v" ] && echo "$v" || echo "n/a"
}
wait_alive() { # pid secs label
    local pid=$1 secs=$2 label=$3 i
    for i in $(seq 1 $((secs * 10))); do
        kill -0 "$pid" 2>/dev/null || return 1
        sleep 0.1
    done
    return 0
}
wait_exit() { # pid timeout_secs
    local pid=$1 secs=$2 i
    for i in $(seq 1 $((secs * 10))); do
        kill -0 "$pid" 2>/dev/null || return 0
        sleep 0.1
    done
    return 1
}

# ---------- main ------------------------------------------------------------------
main() {
    mkdir -p "$PERF_DIR" || die "cannot create $PERF_DIR"
    case "$MODE" in
        old|new) ;;
        *) die "MODE must be old|new (got '$MODE')" ;;
    esac
    local probe_bin
    if [ "$MODE" = "old" ]; then probe_bin=$PROBE_OLD; else probe_bin=$PROBE_NEW; fi
    [ -x "$probe_bin" ] || die "probe binary not found: $probe_bin (MODE=$MODE)"
    [ -x "$GEN" ] || compile_gen
    compile_gen

    local tag=${RUN_TAG:-$MODE}
    local samples=/tmp/perf-$tag-samples.csv
    local agg=/tmp/perf-$tag-agg.csv
    local plog=$PERF_DIR/perf-$tag-probe.log
    local slog=$PERF_DIR/perf-$tag-send.log
    local rlog=$PERF_DIR/perf-$tag-recv.log
    local summary=$PERF_DIR/perf-$tag-summary.txt
    local tsv=$PERF_DIR/perf-summary.tsv
    rm -f "$samples" "$agg" "$plog" "$slog" "$rlog" "$summary"

    log "=== run_perf MODE=$MODE ==="
    log "probe=$probe_bin iface=$IFACE host=$HOST_IP ns=$NS ns_ip=$NS_IP port=$PORT"
    log "target pps=$PPS dur=${DUR}s payload=$PAYLOAD dports=$DPORTS -> expected ~$((PPS * DUR)) packets"
    log "gen=$GEN tag=$tag"

    compile_gen
    ensure_netns

    # --- start probe
    local -a pargs=()
    if [ "$MODE" = "old" ]; then
        pargs=(--iface "$IFACE" --out "$samples" --flush-ms "$FLUSH_MS")
    else
        pargs=(--iface "$IFACE" --out "$samples" --agg-out "$agg" \
               --window-ms "$WINDOW_MS" --grace-ms "$GRACE_MS" --scan-ms "$SCAN_MS" \
               --flush-ms "$FLUSH_MS")
    fi
    local -a prob_env=()
    [ -n "${PROBE_RUST_LOG:-}" ] && prob_env=(env "RUST_LOG=$PROBE_RUST_LOG")
    "${prob_env[@]}" "$probe_bin" "${pargs[@]}" >"$plog" 2>&1 &
    PROBE_PID=$!
    local probe_pid_used=$PROBE_PID
    log "probe started pid=$PROBE_PID args=${pargs[*]}"
    if ! wait_alive "$PROBE_PID" 4 "probe"; then
        cat "$plog" >&2 || true
        die "probe exited during attach"
    fi
    # Attach grace: give the XDP program time to attach before traffic.
    sleep 0.8
    bpftool net list 2>/dev/null | head -6

    # --- start receiver (keeps the port bound, avoids ICMP unreachable)
    local recv_secs=$((DUR + 4))
    "$GEN" recv --bind "$DST" --secs "$recv_secs" >"$rlog" 2>&1 &
    RECV_PID=$!
    sleep 0.3
    kill -0 "$RECV_PID" 2>/dev/null || { cat "$rlog" >&2; die "recv failed to start"; }

    # --- measure
    local clk_tck; clk_tck=${CLK_TCK:-$(getconf CLK_TCK)}
    local ns0=0 jiff0=0 thr0=0 ns1=0 jiff1=0 thr1=0
    if ! kill -0 "$PROBE_PID" 2>/dev/null; then
        cat "$plog" >&2 || true
        die "probe died before traffic started"
    fi
    ns0=$(proc_cpu_ns "$PROBE_PID")
    jiff0=$(proc_cpu_ticks_all "$PROBE_PID")
    thr0=$(proc_threads "$PROBE_PID")
    local l0u=0 l0s=0 l1u=0 l1s=0
    read -r l0u l0s <<<"$(proc_leader_ticks "$PROBE_PID")"
    local probe_cmd; probe_cmd=$(tr '\0' ' ' <"/proc/$PROBE_PID/cmdline" 2>/dev/null | cut -c1-160)
    log "probe cpu baseline: pid=$PROBE_PID threads=$thr0 ticks=$jiff0 ns=$ns0 leader=$((l0u + l0s))"
    log "probe cmdline: $probe_cmd"
    local sys0; sys0=$(procstat_busy)
    local udp0; udp0=$(udp_in_errors)
    local t0_ns; t0_ns=$(date +%s%N)

    log "sending: $PPS pps for ${DUR}s from $NS to $DST"
    ip netns exec "$NS" "$GEN" send --dst "$DST" --dports "$DPORTS" --pps "$PPS" --secs "$DUR" --payload "$PAYLOAD" >"$slog" 2>&1 &
    SEND_PID=$!
    wait "$SEND_PID"; local send_rc=$?
    SEND_PID=""
    local t1_ns; t1_ns=$(date +%s%N)
    ns1=$(proc_cpu_ns "$PROBE_PID")
    jiff1=$(proc_cpu_ticks_all "$PROBE_PID")
    thr1=$(proc_threads "$PROBE_PID")
    read -r l1u l1s <<<"$(proc_leader_ticks "$PROBE_PID")"
    local sys1; sys1=$(procstat_busy)
    local udp1; udp1=$(udp_in_errors)
    [ "$send_rc" = "0" ] || log "WARN: sender rc=$send_rc"
    cat "$slog"

    # --- drain: give the ring one flush period, then stop the probe gracefully.
    # SIGINT is sent while the aggregation window is still open, so the exit path
    # itself must flush the open window (that is what A5/A9 exercise).
    local drain_ms=$((FLUSH_MS + 500))
    sleep "$(awk -v ms="$drain_ms" 'BEGIN{printf "%.1f", ms/1000}')"
    local probe_exit_ms=0
    probe_exit_ms=$(sigint_and_time "$PROBE_PID")
    PROBE_PID=""
    wait_exit "$RECV_PID" 3 >/dev/null 2>&1
    kill_pid "$RECV_PID" recv
    RECV_PID=""

    # --- gather numbers
    local wall_ms=$(( (t1_ns - t0_ns) / 1000000 ))
    local sent elapsed_ms send_pps send_errs
    sent=$(sed -n 's/.*sent=\([0-9]*\).*/\1/p' "$slog" | tail -1); sent=${sent:-0}
    elapsed_ms=$(sed -n 's/.*elapsed_ms=\([0-9]*\).*/\1/p' "$slog" | tail -1); elapsed_ms=${elapsed_ms:-0}
    send_pps=$(sed -n 's/.*pps=\([0-9]*\).*/\1/p' "$slog" | tail -1); send_pps=${send_pps:-0}
    send_errs=$(sed -n 's/.*send_errors=\([0-9]*\).*/\1/p' "$slog" | tail -1); send_errs=${send_errs:-0}
    local recv_n recv_pps
    recv_n=$(sed -n 's/^received=\([0-9]*\).*/\1/p' "$rlog" | tail -1); recv_n=${recv_n:-0}
    recv_pps=$(sed -n 's/^received=.*pps=\([0-9]*\).*/\1/p' "$rlog" | tail -1); recv_pps=${recv_pps:-0}

    local rows rows_udp rows_v6 proto_counts
    rows=$(data_rows "$samples")
    rows_udp=$(csv_udp_rows "$samples")
    rows_v6=$(csv_v6_rows "$samples")
    proto_counts=$(csv_proto_counts "$samples")
    [ -z "$proto_counts" ] && proto_counts="none"
    local rows_per_sec; rows_per_sec=$(awk -v r="$rows" -v ms="$wall_ms" 'BEGIN{ if(ms>0) printf "%.0f", r*1000/ms; else print 0 }')
    local stats_line; stats_line=$(last_stats_line "$plog")
    local ev_rows kernel_dropped malformed sample_failed
    ev_rows=$(field_of "$stats_line" events_received)
    kernel_dropped=$(field_of "$stats_line" kernel_dropped)
    malformed=$(field_of "$stats_line" malformed)
    sample_failed=$(field_of "$stats_line" sample_failed)
    local dropped_field="${kernel_dropped}/${sample_failed}"

    # New-implementation counters (the old stats line only carries
    # events_received / rows_written / kernel_dropped / malformed).
    local packets_total sample_rows agg_records late_dropped samples_deduped stats_agg_overflow
    packets_total=$(field_of "$stats_line" packets_total)
    sample_rows=$(field_of "$stats_line" sample_rows)
    agg_records=$(field_of "$stats_line" agg_records)
    late_dropped=$(field_of "$stats_line" late_dropped)
    samples_deduped=$(field_of "$stats_line" samples_deduped)
    stats_agg_overflow=$(field_of "$stats_line" agg_overflow)
    local rows_written; rows_written=$(field_of "$stats_line" rows_written)
    local metrics_line metrics_agg_keys metrics_scan_ms metrics_scan_interval_ms
    metrics_line=$(grep -E 'metrics:' "$plog" 2>/dev/null | tail -1)
    metrics_agg_keys=$(field_of "$metrics_line" agg_keys)
    metrics_scan_ms=$(field_of "$metrics_line" scan_ms)
    metrics_scan_interval_ms=$(field_of "$metrics_line" scan_interval_ms)

    # Aggregation CSV detail (16 fixed columns, see probe/src/sink.rs AggCsvSink::HEADER:
    # 1 window_start, 2 window_end, 3 iface, 4 direction, 5 src_prefix, 6 dst_addr,
    # 7 dst_port, 8 protocol, 9 action, 10 rule_id, 11 count, 12 first_seen, 13 last_seen,
    # 14 sample_dropped, 15 degraded, 16 window_complete).
    local agg_rows=0 agg_count=0 agg_unique_keys=0 agg_unique_ports=0
    local agg_sample_dropped_rows=0 agg_degraded_rows=0 agg_complete_rows=0 agg_open_rows=0
    if [ -s "$agg" ]; then
        agg_rows=$(data_rows "$agg")
        agg_count=$(agg_sum_count "$agg")
        agg_unique_ports=$(awk -F, 'NR>1 {print $7}' "$agg" | sort -u | wc -l)
        agg_unique_keys=$(awk -F, 'NR>1 {print $5","$6","$7","$8","$9","$10}' "$agg" | sort -u | wc -l)
        agg_sample_dropped_rows=$(awk -F, 'NR>1 && $14==1 {n++} END{print n+0}' "$agg")
        agg_degraded_rows=$(awk -F, 'NR>1 && $15==1 {n++} END{print n+0}' "$agg")
        agg_complete_rows=$(awk -F, 'NR>1 && $16==1 {n++} END{print n+0}' "$agg")
        agg_open_rows=$((agg_rows - agg_complete_rows))
    fi

    # CPU: official metric is /proc/<pid>/stat utime+stime per the A9 spec.  The probe
    # runs a 21-thread runtime, so utime+stime is summed over /proc/<pid>/task/*/stat
    # (leader-only value and /proc/<pid>/schedstat ns-resolution value are cross-checks;
    # CLK_TCK=100 means one jiffy = 10 ms = 0.2 % of a 5 s window).
    local cpu_ns_delta=$((ns1 - ns0))
    local jiff_delta=$((jiff1 - jiff0))
    local leader_delta=$(( (l1u - l0u) + (l1s - l0s) ))
    local cpu_pct cpu_pct_schedstat cpu_pct_leader cpu_pct_source
    cpu_pct=$(awk -v d="$jiff_delta" -v hz="$clk_tck" -v ms="$wall_ms" \
        'BEGIN{ if(ms>0) printf "%.2f", d/hz*1000/ms*100; else print "n/a" }')
    cpu_pct_schedstat=$(awk -v dns="$cpu_ns_delta" -v ms="$wall_ms" \
        'BEGIN{ if(ms>0) printf "%.3f", dns/1e9*1000/ms*100; else print "n/a" }')
    cpu_pct_leader=$(awk -v d="$leader_delta" -v hz="$clk_tck" -v ms="$wall_ms" \
        'BEGIN{ if(ms>0) printf "%.2f", d/hz*1000/ms*100; else print "n/a" }')
    cpu_pct_source="proc-stat-utime+stime"
    local sys_cpu_pct nproc
    nproc=$(getconf _NPROCESSORS_ONLN 2>/dev/null || echo 1)
    sys_cpu_pct=$(awk -v d=$((sys1 - sys0)) -v hz="$clk_tck" -v ms="$wall_ms" -v n="$nproc" \
        'BEGIN{ if(ms>0) printf "%.1f", d/hz*1000/ms*100/n; else print "n/a" }')

    local counted count_label count_err
    if [ "$MODE" = "old" ]; then
        counted=$rows
        count_label="$rows rows"
        count_err=$(awk -v s="$sent" -v c="$counted" 'BEGIN{ if(s>0) printf "%.2f%%", (s-c)*100/s; else print "n/a" }')
    else
        counted=$agg_count
        count_label="sum_count=$counted (agg_rows=$agg_rows)"
        count_err=$(awk -v s="$sent" -v c="$counted" 'BEGIN{ if(s>0) printf "%.2f%%", (s-c)*100/s; else print "n/a" }')
    fi

    # A5-style acceptance booleans (evaluated on the aggregation CSV + stats line).
    local check_count_err=0 check_overflow=0 check_sample_failed=0 check_sample_dropped=0
    if [ "$MODE" = "new" ] && [ "$sent" -gt 0 ]; then
        check_count_err=$(awk -v s="$sent" -v c="$counted" \
            'BEGIN{ e=(s-c)*100/s; if(e<0)e=-e; print (e<=1.0)?1:0 }')
    fi
    [ "$stats_agg_overflow" = "0" ] && check_overflow=1
    case "$sample_failed" in
        ''|n/a) ;;
        *) [ "$sample_failed" -gt 0 ] 2>/dev/null && check_sample_failed=1 ;;
    esac
    [ "$agg_sample_dropped_rows" -gt 0 ] && check_sample_dropped=1

    local note=""
    if [ "$sent" -lt "$((PPS * DUR / 2))" ]; then
        note="sender only reached ${send_pps}pps"
    fi
    [ "$rows_v6" -gt 0 ] && note="${note:+$note; }WARN ipv6 rows in samples ($rows_v6)"
    [ "$recv_n" -lt "$sent" ] && note="${note:+$note; }recv(reference only) ${recv_n}/${sent}"

    # --- artifacts
    local run_id="${RUN_ID:-$tag-$(date +%Y%m%dT%H%M%S)}"
    {
        echo "run_id=$run_id"
        echo "mode=$MODE"
        echo "probe=$probe_bin"
        echo "iface=$IFACE args=${pargs[*]}"
        echo "ipv6_disabled_at_start=$IPV6_STATE"
        echo "ipv6_disabled_at_end=$IFACE:$(cat /proc/sys/net/ipv6/conf/$IFACE/disable_ipv6 2>/dev/null || echo n/a) $PEER:$(ip netns exec "$NS" cat /proc/sys/net/ipv6/conf/$PEER/disable_ipv6 2>/dev/null || echo n/a)"
        echo "traffic=target_pps=$PPS dur_s=$DUR payload=$PAYLOAD dports=$DPORTS base_port=$PORT dst=$DST sender_ns=$NS"
        echo "sent=$sent sender_elapsed_ms=$elapsed_ms sender_pps=$send_pps send_errors=$send_errs"
        echo "recv_received=$recv_n recv_pps=$recv_pps (reference only; sender 'sent' is the ground truth)"
        echo "wall_window_ms=$wall_ms probe_exit_ms=$probe_exit_ms (SIGINT -> process exit)"
        echo "probe_threads=$thr0->$thr1 probe_cpu_source=$cpu_pct_source"
        echo "probe_pid=$probe_pid_used probe_cmdline=$probe_cmd"
        echo "probe_cpu_ns_delta=$cpu_ns_delta probe_cpu_ticks_delta=$jiff_delta probe_leader_ticks_delta=$leader_delta clk_tck=$clk_tck"
        echo "cpu_pct=$cpu_pct probe_cpu_pct=$cpu_pct (all threads, utime+stime)"
        echo "cpu_pct_schedstat=$cpu_pct_schedstat cpu_pct_leader=$cpu_pct_leader"
        echo "system_cpu_pct=$sys_cpu_pct (percent of total capacity across $nproc cpus, /proc/stat busy delta)"
        echo "samples_csv=$samples data_rows=$rows rows_per_sec=$rows_per_sec"
        echo "samples_csv_udp_rows=$rows_udp samples_csv_ipv6_rows=$rows_v6 proto_rows=[$proto_counts]"
        if [ "$MODE" = "new" ]; then
            echo "agg_csv=$agg agg_rows=$agg_rows sum_count=$counted agg_unique_keys=$agg_unique_keys agg_unique_dst_ports=$agg_unique_ports"
            echo "agg_rows_sample_dropped=$agg_sample_dropped_rows agg_rows_degraded=$agg_degraded_rows agg_rows_window_complete=$agg_complete_rows agg_rows_window_open=$agg_open_rows"
        fi
        echo "counted=$count_label"
        echo "count_error=$count_err"
        echo "stats_fields=packets_total:$packets_total sample_rows:$sample_rows events_received:$ev_rows agg_records:$agg_records rows_written:$rows_written kernel_dropped:$kernel_dropped sample_failed:$sample_failed agg_overflow:$stats_agg_overflow malformed:$malformed late_dropped:$late_dropped samples_deduped:$samples_deduped"
        echo "metrics_line=$metrics_line"
        echo "metrics_agg_keys=$metrics_agg_keys metrics_scan_ms=$metrics_scan_ms metrics_scan_interval_ms=$metrics_scan_interval_ms"
        echo "a5_check=count_error_le_1pct:$check_count_err agg_overflow_zero:$check_overflow sample_failed_gt_0:$check_sample_failed sample_dropped_gt_0:$check_sample_dropped"
        echo "udp_receive_errors_delta=$((udp1 - udp0))"
        echo "stats_line=$stats_line"
        echo "probe_log=$plog send_log=$slog recv_log=$rlog"
        [ -s "$samples" ] && echo "samples_csv_head=$(head -2 "$samples" | tail -1)"
        [ -s "$agg" ] && echo "agg_csv_head=$(head -2 "$agg" | tail -1)"
    } >"$summary"

    # keep the raw CSV only when it is small enough to be a useful artifact
    local csv_bytes; csv_bytes=$(stat -c %s "$samples" 2>/dev/null || echo 0)
    if [ "$csv_bytes" -le 5242880 ]; then
        cp -f "$samples" "$PERF_DIR/perf-$tag-samples.csv"
        echo "samples_csv_artifact=$PERF_DIR/perf-$tag-samples.csv (full, $csv_bytes B)" >>"$summary"
    else
        { head -5 "$samples"; echo "..."; tail -2 "$samples"; } >"$PERF_DIR/perf-$tag-samples.head.csv"
        echo "samples_csv_artifact=$PERF_DIR/perf-$tag-samples.head.csv (head only, full CSV $csv_bytes B in $samples)" >>"$summary"
    fi
    if [ "$MODE" = "new" ] && [ -s "$agg" ]; then
        cp -f "$agg" "$PERF_DIR/perf-$tag-agg.csv"
        echo "agg_csv_artifact=$PERF_DIR/perf-$tag-agg.csv" >>"$summary"
    fi

    # --- table row + stdout
    if [ ! -s "$tsv" ]; then
        printf 'run_id\ttimestamp\timpl\tsent\trows_or_sumcount\trows_per_sec\tcount_error\tprobe_cpu_pct\tprobe_cpu_pct_schedstat\tkernel_dropped/sample_failed\trecv_received(reference)\tnote\n' >"$tsv"
    fi
    printf '%s\t%s\t%s\t%s\t%s\t%s\t%s\t%s\t%s\t%s\t%s\t%s\n' \
        "$run_id" "$(ts)" "$MODE" "$sent" "${count_label// /_}" "$rows_per_sec" "$count_err" "$cpu_pct" \
        "$cpu_pct_schedstat" "${dropped_field// /_}" "$recv_n" "${note//$'\t'/ }" >>"$tsv"

    echo
    echo "===== A9 perf result run_id=$run_id ($(ts)) ====="
    printf '| %-4s | %-8s | %-16s | %-10s | %-9s | %-11s | %-22s | %-6s |\n' \
        "impl" "sent" "rows/sum_count" "rows/s" "count_err" "probe_cpu%" "kernel_dropped/sample_failed" "recv*"
    printf '| %-4s | %-8s | %-16s | %-10s | %-9s | %-11s | %-22s | %-6s |\n' \
        "----" "----" "--------------" "------" "---------" "-----------" "------------------------" "----"
    printf '| %-4s | %-8s | %-16s | %-10s | %-9s | %-11s | %-22s | %-6s |\n' \
        "$MODE" "$sent" "${counted}" "$rows_per_sec" "$count_err" "$cpu_pct" "$dropped_field" "$recv_n"
    echo "(* recv = UDP receiver on the host; reference only, 'sent' from the sender is the ground truth)"
    echo
    echo "sender: sent=$sent wall=${elapsed_ms}ms measured_pps=$send_pps target_pps=$PPS dports=$DPORTS send_errors=$send_errs"
    echo "receiver: received=$recv_n measured_pps=$recv_pps udp_receive_errors_delta=$((udp1 - udp0))  [reference only]"
    echo "probe: wall_window=${wall_ms}ms cpu=${cpu_pct}% [$cpu_pct_source, sum of all $thr1 threads; schedstat=${cpu_pct_schedstat}%; leader-only=${cpu_pct_leader}%; BPF softirq time excluded]"
    echo "system_wide_cpu=${sys_cpu_pct}% of total capacity across $nproc cpus"
    echo "samples_csv=$samples data_rows=$rows (udp=$rows_udp ipv6=$rows_v6) rows_per_sec=$rows_per_sec proto=[$proto_counts]"
    echo "probe SIGINT->exit=${probe_exit_ms}ms  metrics_scan_ms=$metrics_scan_ms scan_interval_ms=$metrics_scan_interval_ms agg_keys(metrics)=$metrics_agg_keys"
    [ "$MODE" = "new" ] && echo "agg_csv=$agg agg_rows=$agg_rows sum_count=$counted unique_keys=$agg_unique_keys unique_dst_ports=$agg_unique_ports sample_dropped_rows=$agg_sample_dropped_rows degraded_rows=$agg_degraded_rows window_complete=$agg_complete_rows window_open=$agg_open_rows"
    echo "probe stats: packets_total=$packets_total sample_rows=$sample_rows events_received=$ev_rows agg_records=$agg_records kernel_dropped=$kernel_dropped sample_failed=$sample_failed agg_overflow=$stats_agg_overflow malformed=$malformed late_dropped=$late_dropped samples_deduped=$samples_deduped"
    echo "count_error=$count_err  kernel_dropped=$kernel_dropped  sample_failed=$sample_failed  malformed=$malformed"
    [ "$MODE" = "new" ] && echo "A5 checks: count_error<=1%:$check_count_err agg_overflow==0:$check_overflow sample_failed>0:$check_sample_failed sample_dropped>0:$check_sample_dropped"
    echo "stats_line=$stats_line"
    echo "artifacts: $summary , $tsv , $plog , $slog , $rlog"
    [ -n "$note" ] && echo "NOTE: $note"
    echo "====================================="
}

main "$@"
