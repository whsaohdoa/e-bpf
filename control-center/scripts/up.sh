#!/usr/bin/env bash
# Bring up the compose stack and block until both services answer their readiness probe.
set -euo pipefail
source "$(dirname "$0")/lib.sh"

log "building images and starting containers"
docker compose up -d --build

log "waiting for cc-mgmt readiness"
wait_http "$MGMT_URL/actuator/health/readiness" 120 || fail "cc-mgmt did not become ready; try: docker compose logs cc-mgmt"
ok "cc-mgmt ready"

log "waiting for cc-control readiness"
wait_http "$CONTROL_URL/actuator/health/readiness" 120 || fail "cc-control did not become ready; try: docker compose logs cc-control"
ok "cc-control ready"

log "waiting for cc-web"
wait_http "$WEB_URL/healthz" 60 || fail "cc-web did not become ready; try: docker compose logs cc-web"
ok "cc-web ready"

cat <<EOF

  SPA        $WEB_URL
  mgmt API   $MGMT_URL/api/health
  control    $CONTROL_URL/api/health   (direct access exists only on the host loopback;
                                        in-cluster it is blocked by NetworkPolicy)
  postgres   127.0.0.1:${PG_PORT}      (postgres/cc_migrator/cc_app/cc_audit/cc_ro)

  next: bash scripts/smoke.sh
        bash scripts/p95.sh 50
        bash scripts/migrate.sh mgmt 1
EOF
