#!/usr/bin/env bash
# Interactive psql against the compose database.
#   bash scripts/psql.sh mgmt
#   bash scripts/psql.sh mgmt -c "select count(*) from mgmt.machine"
# Use CC_PSQL_USER=cc_migrator to inspect DDL, cc_audit/cc_ro to prove the read-only roles.
set -euo pipefail
source "$(dirname "$0")/lib.sh"

db="${1:?usage: psql.sh <mgmt|control> [psql args...]}"
shift || true

exec docker compose exec cc-pg psql -U "${CC_PSQL_USER:-cc_app}" -d "$db" "$@"
