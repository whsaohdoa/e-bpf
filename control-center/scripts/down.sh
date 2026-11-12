#!/usr/bin/env bash
# Stop the stack. `--wipe` also removes the Postgres volume, which is the only way to
# re-run the initdb scripts (roles/databases/schemas are created there, not by Liquibase).
set -euo pipefail
source "$(dirname "$0")/lib.sh"

if [[ "${1:-}" == "--wipe" ]]; then
    log "removing containers and the cc-pgdata volume"
    docker compose down -v
else
    log "removing containers (data volume kept)"
    docker compose down
fi
