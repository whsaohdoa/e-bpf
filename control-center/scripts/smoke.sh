#!/usr/bin/env bash
# Post-up assertions: the SPA's proxy path, both roles, and the seed data.
set -euo pipefail
source "$(dirname "$0")/lib.sh"

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
ok "smoke passed"
