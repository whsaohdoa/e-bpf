#!/usr/bin/env bash
# Post-up assertions: the SPA's proxy path, both roles, the seed data, and the
# role split on the agent channel (only cc.role=control opens 9090).
#
# Runs on the host against compose's published loopback ports by default. The same
# script runs from a container attached to the compose network when the probe
# targets are pointed at service names, e.g.
#
#   CC_WEB_URL=http://cc-train0-cc-web:80 \
#   CC_MGMT_HOST=cc-train0-cc-mgmt-1:8080 \
#   CC_CONTROL_HOST=cc-train0-cc-control-1:8080 \
#   CC_GRPC_PORT=9090 bash scripts/smoke.sh
#
# CC_MGMT_URL / CC_CONTROL_URL / CC_WEB_URL keep their lib.sh meaning; when only
# CC_MGMT_HOST / CC_CONTROL_HOST is given, the REST URLs are rebuilt from them.
set -euo pipefail
source "$(dirname "$0")/lib.sh"

# --- probe targets ---------------------------------------------------------
# Loopback defaults mirror what compose publishes (mgmt 8081, control 8082,
# control gRPC 9090); inside the network the ports are 8080/8080/9090.
strip_scheme() { local u=$1; u=${u#*://}; printf '%s' "${u%%/*}"; }
split_addr() {  # split_addr host[:port] default_port -> "host port"
    local a=$1 port=$2
    if [[ $a == *:* ]]; then
        printf '%s %s\n' "${a%:*}" "${a##*:}"
    else
        printf '%s %s\n' "$a" "$port"
    fi
}
MGMT_ADDR="${CC_MGMT_HOST:-$(strip_scheme "$MGMT_URL")}"
CONTROL_ADDR="${CC_CONTROL_HOST:-$(strip_scheme "$CONTROL_URL")}"
read -r MGMT_HOST MGMT_PORT <<<"$(split_addr "$MGMT_ADDR" 8080)"
read -r CONTROL_HOST CONTROL_PORT <<<"$(split_addr "$CONTROL_ADDR" 8080)"
# The gRPC endpoint lives on the control host; CC_GRPC_HOST is an escape hatch.
GRPC_HOST="${CC_GRPC_HOST:-$CONTROL_HOST}"
GRPC_PORT="${CC_GRPC_PORT:-9090}"
# Port the server binds *inside* cc-control; only differs when the host publishes a
# non-default CC_CONTROL_GRPC_PORT. Used for the cc-mgmt socket-table check.
GRPC_INNER_PORT="${CC_GRPC_INNER_PORT:-9090}"
[[ -n "${CC_MGMT_URL:-}" ]] || MGMT_URL="http://$MGMT_HOST:$MGMT_PORT"
[[ -n "${CC_CONTROL_URL:-}" ]] || CONTROL_URL="http://$CONTROL_HOST:$CONTROL_PORT"

# port_open HOST PORT -- plain bash /dev/tcp; the images have neither nc nor ss,
# and this also resolves Docker's embedded DNS from inside the compose network.
port_open() {
    local host=$1 port=$2
    (exec 3<>"/dev/tcp/$host/$port") 2>/dev/null
}

# mgmt_listens_on_grpc -- true when the management instance opened the agent port.
mgmt_listens_on_grpc() {
    # Network run: cc-mgmt is a distinct host, so probing its 9090 is conclusive.
    if [[ $MGMT_HOST != "127.0.0.1" && $MGMT_HOST != "localhost" ]]; then
        port_open "$MGMT_HOST" "$GRPC_PORT"
        return
    fi
    # Host run: both roles sit behind 127.0.0.1 and 9090 is cc-control's published
    # port, so a loopback probe cannot tell the roles apart. Ask the cc-mgmt
    # container's socket table instead (9090 == 0x2382 in /proc/net/tcp).
    local container hex
    if ! container="$(docker compose ps -q cc-mgmt 2>/dev/null)"; then
        fail "cannot inspect cc-mgmt on the host (docker compose unusable); run from the compose network with CC_MGMT_HOST"
    fi
    [[ -n "$container" ]] || fail "cc-mgmt is not running; start the stack with scripts/up.sh first"
    hex="$(printf '%04x' "$GRPC_INNER_PORT")"
    docker compose exec -T cc-mgmt sh -c "cat /proc/net/tcp /proc/net/tcp6 2>/dev/null | grep -q ':${hex}'"
}

log "GET $WEB_URL/api/health (through the nginx proxy)"
body="$(curl -fsS "$WEB_URL/api/health")" || fail "SPA proxy did not reach the management API"
echo "    $body"
echo "$body" | grep -q '"role":"mgmt"' || fail "expected role=mgmt behind the SPA proxy"
ok "SPA -> mgmt proxy works"

log "GET $WEB_URL/api/machines"
machines="$(curl -fsS "$WEB_URL/api/machines")" || fail "machines endpoint failed"
echo "$machines" | grep -q '"id":"m1"' || fail "seed data missing (m1)"
echo "$machines" | grep -q '"group_id":"g2"' || fail "seed data missing (g2)"
ok "seed machines present"

log "GET $CONTROL_URL/api/health"
chealth="$(curl -fsS "$CONTROL_URL/api/health")" || fail "control plane health failed"
echo "    $chealth"
echo "$chealth" | grep -q '"role":"control"' || fail "expected role=control"

code="$(curl -s -o /dev/null -w '%{http_code}' "$CONTROL_URL/api/machines")"
[[ "$code" == "404" ]] || fail "control plane must not expose the management read model (got HTTP $code)"
ok "control plane does not serve /api/machines"

log "GET $MGMT_URL/actuator/health"
curl -fsS "$MGMT_URL/actuator/health" | grep -q '"status":"UP"' || fail "actuator health is not UP"
ok "actuator health is UP"

# Agent channel: the gRPC server is cc.role=control only. The management role has no
# BindableService, so spring-grpc never opens 9090 there -- a regression would mean
# the role gate broke, not just a port conflict.
log "probe $GRPC_HOST:$GRPC_PORT (control plane must listen)"
if port_open "$GRPC_HOST" "$GRPC_PORT"; then
    ok "control plane listens on $GRPC_HOST:$GRPC_PORT"
else
    fail "control plane is not listening on $GRPC_HOST:$GRPC_PORT (gRPC server did not start?)"
fi

log "probe $MGMT_HOST:$GRPC_PORT (management plane must stay closed)"
if mgmt_listens_on_grpc; then
    fail "management plane must not listen on $GRPC_PORT (cc.role=mgmt leaked the gRPC server)"
fi
ok "management plane does not listen on $GRPC_PORT"

echo "smoke passed"
