#!/usr/bin/env bash
# verification/run_control_demo.sh — G1: "publish a new configuration, the agent
# applies it and acknowledges it", measured end to end on a **real** `probe-agent`
# driven by the in-process mock control plane (never a test double).
#
# Contract: docs/Control-Agent-API-v1.md §9 (G1) and §10 (L1-L6).
#
# Topology:
#
#   probe-agent (root, XDP on lo)  --gRPC + mTLS-->  control-mock (127.0.0.1:N)
#          |                                              |
#          +-- state dir (applied.json)                   +-- HTTP admin (N+1)
#
# Two phases, five measured publishes each:
#
#   push  the mock supports `Watch`, so a publish is pushed to the agent and the
#         ack comes back within milliseconds                        watch_used=1
#   poll  a fresh mock does not support `Watch`, so the agent finds the new
#         version on its next heartbeat                            watch_used=0
#
# After the two phases the agent is killed and restarted against the same control
# plane: that is the S1-7 "restart recovery" evidence — it restores its applied
# configuration and resumes at the control plane's cursor + 1.
#
# Output goes to the workspace's outer `verification/` directory, which is not
# part of the repository:
#
#   verification/control/g1.csv          one row per measured publish
#   verification/control/mock-*.log      mock control plane log per phase
#   verification/control/agent-*.log     probe-agent log per phase
#   verification/control/acks-*.json     the admin view the CSV was built from
#   verification/control/state-*.json    cursor, call counts, reported windows
#
# Usage (inside the aya-dev container, as root — XDP attach needs it):
#
#   bash verification/run_control_demo.sh
#
# Knobs:
#   CONTROL_DEMO_OUT=DIR   where the CSV and logs go (default: ../verification/control)
#   CONTROL_DEMO_KEEP=1    keep the temporary directory (certs, mock configs)
#
# Exit code: 0 only when all ten publishes were acknowledged as APPLIED.

set -uo pipefail

REPO=$(cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd)
OUT=${CONTROL_DEMO_OUT:-$(cd "$REPO/.." && pwd)/verification/control}
BIN=$REPO/target/debug
CSV=""

# Contract §6.4 document B: 10000 ms window, 2000 ms grace, /24 and /64 prefixes,
# one `r-8080` BLOCK rule. 63 bytes; the contract pins its sha256. Each measured
# publish flips the two-byte `grace_ms` varint (tag `18`), so every version serves
# *different bytes*: the agent must actually download and apply, instead of taking
# `sync_config`'s same-hash shortcut (§6.3).
DOC_B=080110904e18d00f201828403a3108011206722d38303830180a20012a1f080410061a0b31302e302e312e302f3234220831302e302e322e352a02903f3001

GRPC_PORT=0
ADMIN_PORT=0
MOCK_PID=""
AGENT_PID=""
WORK=""

die() {
    echo "run_control_demo: $*" >&2
    exit 1
}

cleanup() {
    stop_agent
    stop_mock
    if [[ -n $WORK && ${CONTROL_DEMO_KEEP:-0} != 1 ]]; then
        rm -rf "$WORK"
    elif [[ -n $WORK ]]; then
        echo "kept $WORK"
    fi
}
trap cleanup EXIT

stop_agent() {
    [[ -n $AGENT_PID ]] || return 0
    kill "$AGENT_PID" 2>/dev/null
    wait "$AGENT_PID" 2>/dev/null
    AGENT_PID=""
    # The kernel drops the XDP program when the process dies; give the detach a
    # moment before the next phase attaches to the same interface.
    sleep 0.3
}

stop_mock() {
    [[ -n $MOCK_PID ]] || return 0
    kill "$MOCK_PID" 2>/dev/null
    wait "$MOCK_PID" 2>/dev/null
    MOCK_PID=""
}

now_ms() {
    date +%s%3N
}

json_num() {
    sed -n "s/.*\"$1\":\([0-9]\+\).*/\1/p" <<<"$2" | head -n 1
}

json_str() {
    sed -n "s/.*\"$1\":\"\([^\"]*\)\".*/\1/p" <<<"$2" | head -n 1
}

json_bool() {
    sed -n "s/.*\"$1\":\(true\|false\).*/\1/p" <<<"$2" | head -n 1
}

json_array() {
    sed -n "s/.*\"$1\":\[\([^]]*\)\].*/\1/p" <<<"$2" | head -n 1
}

http() {
    curl -fsS --max-time 5 "$@"
}

# `doc_hex GRACE_MS` — the §6.4 vector with its grace field replaced. Only the
# two-byte varint for 2000..2095 is supported, which is all this script needs.
doc_hex() {
    local low
    low=$(printf '%02x' $((0xD0 + $1 - 2000)))
    printf '%s\n' "${DOC_B/18d00f/18${low}0f}"
}

port_free() {
    ! (exec 3<>"/dev/tcp/127.0.0.1/$1") 2>/dev/null
}

pick_ports() {
    local attempt
    for attempt in $(seq 1 40); do
        GRPC_PORT=$((19000 + RANDOM % 800))
        ADMIN_PORT=$((GRPC_PORT + 1))
        if port_free "$GRPC_PORT" && port_free "$ADMIN_PORT"; then
            return 0
        fi
    done
    die "could not find a free port pair"
}

build() {
    echo "building the agent binaries ..."
    (cd "$REPO" && cargo build -p agent --features mock --bins) >"$OUT/build.log" 2>&1 ||
        die "cargo build failed; see $OUT/build.log"
}

start_mock() { # $1 config, $2 log, $3 cert dir
    local config=$1 log=$2 certs=$3 deadline
    : >"$log"
    "$BIN/control-mock" \
        --listen "127.0.0.1:$GRPC_PORT" \
        --certs "$certs" \
        --admin-listen "127.0.0.1:$ADMIN_PORT" \
        --config "$config" >"$log" 2>&1 &
    MOCK_PID=$!
    deadline=$(( $(date +%s) + 30 ))
    while (($(date +%s) < deadline)); do
        if http "http://127.0.0.1:$ADMIN_PORT/health" >/dev/null 2>&1; then
            return 0
        fi
        kill -0 "$MOCK_PID" 2>/dev/null || die "control-mock exited; see $log"
        sleep 0.1
    done
    die "control-mock did not answer /health within 30 s; see $log"
}

start_agent() { # $1 cert dir, $2 log
    local certs=$1 log=$2
    : >"$log"
    "$BIN/probe-agent" \
        --iface lo \
        --control-endpoint "https://127.0.0.1:$GRPC_PORT" \
        --ca "$certs/ca.pem" \
        --cert "$certs/client-cert.pem" \
        --key "$certs/client-key.pem" \
        --state-dir "$WORK/state" \
        --window-ms 1000 \
        --grace-ms 200 \
        --flush-ms 200 \
        >>"$log" 2>&1 &
    AGENT_PID=$!
}

wait_log() { # $1 file, $2 pattern, $3 timeout seconds
    local file=$1 pattern=$2 timeout=${3:-20} deadline
    deadline=$(( $(date +%s) + timeout ))
    while (($(date +%s) < deadline)); do
        if grep -q -- "$pattern" "$file" 2>/dev/null; then
            return 0
        fi
        sleep 0.1
    done
    return 1
}

wait_agent_ready() { # $1 log, $2 "registered" | "subscribed"
    local log=$1 need=$2
    wait_log "$log" "registered as" 30 || {
        kill -0 "$AGENT_PID" 2>/dev/null || die "probe-agent exited; see $log"
        die "probe-agent did not register within 30 s; see $log"
    }
    if [[ $need == subscribed ]]; then
        wait_log "$log" "watch: subscribed" 30 ||
            die "probe-agent did not subscribe to Watch; see $log"
    fi
}

publish() { # $1 version, $2 document hex -> the admin response
    local body
    body=$(printf '{"version":%s,"document_hex":"%s"}' "$1" "$2")
    http -X POST "http://127.0.0.1:$ADMIN_PORT/publish" --data "$body"
}

wait_ack() { # $1 version, $2 timeout seconds -> one ack object as JSON
    local version=$1 timeout=${2:-30} deadline body object
    deadline=$(( $(date +%s) + timeout ))
    while (($(date +%s) < deadline)); do
        body=$(http "http://127.0.0.1:$ADMIN_PORT/acks" 2>/dev/null) || body=""
        if [[ -n $body ]]; then
            object=$(tr -d '\n' <<<"$body" | grep -oE '\{[^{}]*\}' | grep -F "\"version\":$version," | head -n 1)
            if [[ -n $object ]]; then
                printf '%s\n' "$object"
                return 0
            fi
        fi
        sleep 0.05
    done
    return 1
}

# record_row ITER TRANSPORT VERSION — publish one version and append its row.
record_row() {
    local iter=$1 transport=$2 version=$3 hex response published ack result
    local received applied watch degraded sha

    hex=$(doc_hex $((2000 + version)))
    response=$(publish "$version" "$hex") ||
        { echo "publish of version $version failed" >&2; return 1; }
    published=$(json_num published_at_ms "$response")
    [[ -n $published ]] ||
        { echo "publish of version $version: no published_at_ms in $response" >&2; return 1; }

    ack=$(wait_ack "$version" 30) ||
        { echo "no ack for version $version within 30 s" >&2; return 1; }
    result=$(json_str result "$ack")
    [[ $result == APPLIED ]] ||
        { echo "version $version was not applied: $ack" >&2; return 1; }

    received=$(json_num received_at_ms "$ack")
    applied=$(json_num applied_at_ms "$ack")
    watch=$(json_bool watch_used "$ack")
    degraded=$(json_array degraded_features "$ack" | tr -d '"' | tr ',' '+')
    sha=$(json_str content_sha256 "$ack")
    [[ $sha == "$(json_str sha256 "$response")" ]] ||
        { echo "version $version: the agent hashed $sha, the control plane served $(json_str sha256 "$response")" >&2; return 1; }

    printf '%s,%s,%s,%s,%s,%s,%s,%s\n' \
        "$iter" "$transport" "$([[ $watch == true ]] && echo 1 || echo 0)" \
        "$published" "$received" "$((received - published))" "$((applied - published))" \
        "${degraded:--}" >>"$CSV"
}

snapshot() { # $1 phase name — the admin views the report quotes
    http "http://127.0.0.1:$ADMIN_PORT/acks" >"$OUT/acks-$1.json" 2>/dev/null || true
    http "http://127.0.0.1:$ADMIN_PORT/state" >"$OUT/state-$1.json" 2>/dev/null || true
}

# S1-7 #1: restart recovery. The control plane owns the cursor, so a restarted
# agent must ask for it and resume at cursor + 1 instead of replaying or skipping.
# The control plane stays up for this step on purpose: the mock keeps its cursor
# in memory, so restarting it would reset the cursor and prove nothing.
restart_recovery() {
    local cursor state resumed deadline
    cursor=$(json_num cursor "$(http "http://127.0.0.1:$ADMIN_PORT/state")")
    [[ -n $cursor ]] || die "the control plane has no cursor to recover from"

    stop_agent
    start_agent "$WORK/certs-poll" "$OUT/agent-restart.log"
    wait_log "$OUT/agent-restart.log" "previous .*applied\.json" 20 ||
        die "the restarted agent did not restore its applied configuration; see $OUT/agent-restart.log"
    wait_log "$OUT/agent-restart.log" "resuming after window $cursor" 20 ||
        die "the restarted agent did not resume after window $cursor; see $OUT/agent-restart.log"

    deadline=$(( $(date +%s) + 20 ))
    while (($(date +%s) < deadline)); do
        state=$(http "http://127.0.0.1:$ADMIN_PORT/state" 2>/dev/null) || state=""
        resumed=$(grep -o '"window_ids":\[[0-9]*' <<<"$state" | grep -o '[0-9]*$' | sort -n | tail -n 1)
        if [[ -n $resumed ]] && ((resumed > cursor)); then
            printf 'restart recovery: cursor was %s, the first window after the restart is %s\n' \
                "$cursor" "$resumed" >>"$OUT/phases.txt"
            snapshot restart
            return 0
        fi
        sleep 0.2
    done
    die "no window beyond $cursor was reported within 20 s of the restart; see $OUT/agent-restart.log"
}

summarize() {
    local rows
    rows=$(( $(wc -l <"$CSV") - 1 ))
    echo
    echo "g1.csv: $rows measured publishes -> $CSV"
    awk -F, 'NR > 1 {
        printf "  iter %-3s %-4s watch_used=%s  publish->ack=%sms  applied_at-publish=%sms  degraded=%s\n",
               $1, $2, $3, $6, $7, $8
    }' "$CSV"
    [[ $rows -eq 10 ]] || die "expected 10 measured rows, got $rows"
    echo
    echo "agent evidence this run produced:"
    grep -hE 'previous .*applied\.json|no .*applied\.json yet|resuming after window|applied config version|acked config version' \
        "$OUT"/agent-*.log | sed 's/^/  /' || true
}

main() {
    [[ $EUID -eq 0 ]] || die "run as root: probe-agent attaches XDP to lo"
    command -v curl >/dev/null || die "curl is required"

    mkdir -p "$OUT"
    CSV=$OUT/g1.csv
    printf 'iter,transport,watch_used,publish_ms,ack_ms,publish_to_ack_ms,agent_apply_ms,degraded\n' >"$CSV"
    : >"$OUT/phases.txt"

    WORK=$(mktemp -d)
    mkdir -p "$WORK/state"

    build
    pick_ports
    echo "mock control plane: gRPC 127.0.0.1:$GRPC_PORT, admin 127.0.0.1:$ADMIN_PORT"
    printf '{"watch_supported":true}\n' >"$WORK/mock-push.json"
    printf '{"watch_supported":false}\n' >"$WORK/mock-poll.json"

    # Phase 1 — push. The mock publishes the §6.4 document as version 1 at
    # startup, so the agent's very first apply is a warm-up, not a measurement.
    start_mock "$WORK/mock-push.json" "$OUT/mock-push.log" "$WORK/certs-push"
    start_agent "$WORK/certs-push" "$OUT/agent-push.log"
    wait_agent_ready "$OUT/agent-push.log" subscribed
    wait_ack 1 30 >/dev/null || die "the agent did not ack the startup document; see $OUT/agent-push.log"
    local iter
    for iter in 2 3 4 5 6; do
        record_row "$iter" push "$iter" || die "push iteration $iter failed"
    done
    snapshot push
    stop_agent
    stop_mock

    # Phase 2 — poll. A fresh mock announces version 1 and supports no Watch,
    # so the agent (which resumed at version 6) only learns about new versions
    # from its heartbeat. Version 7 is the first pull after the restart and is
    # the warm-up for this phase.
    start_mock "$WORK/mock-poll.json" "$OUT/mock-poll.log" "$WORK/certs-poll"
    start_agent "$WORK/certs-poll" "$OUT/agent-poll.log"
    wait_agent_ready "$OUT/agent-poll.log" registered
    # Version 7 is the first pull after the restart, so its latency includes the
    # agent's reconnection; it is this phase's warm-up, not a measurement. The
    # number still matters: the agent was idle, so it waited for the 5 s idle
    # heartbeat before pulling (contract §10 L4).
    local warm_response warm_published warm_ack warm_received
    warm_response=$(publish 7 "$(doc_hex 2007)") ||
        die "the first publish after the restart failed"
    warm_published=$(json_num published_at_ms "$warm_response")
    warm_ack=$(wait_ack 7 30) ||
        die "the agent did not pull version 7 after the restart; see $OUT/agent-poll.log"
    warm_received=$(json_num received_at_ms "$warm_ack")
    printf 'idle-pull: version 7 published at %s ms, acked at %s ms (%s ms later)\n' \
        "$warm_published" "$warm_received" "$((warm_received - warm_published))" >>"$OUT/phases.txt"
    for iter in 8 9 10 11 12; do
        record_row "$iter" poll "$iter" || die "poll iteration $iter failed"
    done
    snapshot poll

    # S1-7 #1: restart recovery. The control plane (not the agent) owns the
    # cursor, so the restarted agent must ask for it and resume at cursor + 1.
    restart_recovery

    summarize
}

main "$@"
