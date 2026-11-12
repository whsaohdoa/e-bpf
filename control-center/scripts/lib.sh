#!/usr/bin/env bash
# Shared helpers for every script. Sourced, never executed.
set -euo pipefail

ROOT="$(cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd)"
cd "$ROOT"

WEB_URL="${CC_WEB_URL:-http://127.0.0.1:8080}"
MGMT_URL="${CC_MGMT_URL:-http://127.0.0.1:8081}"
CONTROL_URL="${CC_CONTROL_URL:-http://127.0.0.1:8082}"
PG_PORT="${CC_PG_PORT:-55432}"
COMPOSE_NET="${CC_COMPOSE_NET:-cc-train0_default}"

log()  { printf '\033[1;34m==>\033[0m %s\n' "$*"; }
warn() { printf '\033[1;33mWARN\033[0m %s\n' "$*"; }
fail() { printf '\033[1;31mFAIL\033[0m %s\n' "$*" >&2; exit 1; }
ok()   { printf '\033[1;32mOK\033[0m %s\n' "$*"; }

# wait_http URL [tries] -- 1s between attempts, no output on success.
wait_http() {
    local url=$1 tries=${2:-60} i
    for ((i = 1; i <= tries; i++)); do
        if curl -fsS -o /dev/null --max-time 3 "$url" 2>/dev/null; then
            return 0
        fi
        sleep 1
    done
    return 1
}
