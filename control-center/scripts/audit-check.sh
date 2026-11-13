#!/usr/bin/env bash
# C5 evidence: the audit table is append-only for the application role. Two independent
# mechanisms are checked -- the trigger (authoritative) and the revoked grants -- and both
# must reject with the trigger message or a permission error, never succeed.
set -euo pipefail
source "$(dirname "$0")/lib.sh"

psql_app() {
    docker compose exec -T cc-pg psql -U cc_app -d mgmt -v ON_ERROR_STOP=1 -q "$@"
}

log "appending a row as cc_app (INSERT must be allowed)"
psql_app -c "INSERT INTO mgmt.audit (actor_type, action, resource_type, result, request_id)
             VALUES ('system', 'smoke.test', 'audit', 'allow', 'req-smoke')"
ok "INSERT allowed"

check_rejected() {
    local sql=$1 label=$2 out rc=0
    out="$(psql_app -c "$sql" 2>&1)" || rc=$?
    [[ $rc -ne 0 ]] || fail "$label succeeded against an append-only table"
    echo "$out" | grep -qE 'append-only|permission denied' \
        || fail "$label failed for an unexpected reason: $out"
    ok "$label rejected: $(echo "$out" | grep -m1 -E 'append-only|permission denied')"
}

check_rejected "UPDATE mgmt.audit SET reason='tampered' WHERE request_id='req-smoke'" "UPDATE"
check_rejected "DELETE FROM mgmt.audit WHERE request_id='req-smoke'" "DELETE"
check_rejected "TRUNCATE mgmt.audit" "TRUNCATE"

log "cleanup is intentionally skipped: the row must stay, that is the point"
