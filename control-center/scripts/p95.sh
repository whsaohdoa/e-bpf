#!/usr/bin/env bash
# Sequential latency sample for the health endpoint. G1 budgets P95 <= 3s for the
# publish -> notify -> ack path; until that path exists, this measures the closest thing
# (a request through the same servlet stack + DB-free handler) so the harness is ready and
# the numbers are comparable run to run.
set -euo pipefail
source "$(dirname "$0")/lib.sh"

N="${1:-50}"
URL="${2:-$MGMT_URL/api/health}"
tmp="$(mktemp)"
trap 'rm -f "$tmp"' EXIT

log "sampling $N requests against $URL"
for ((i = 1; i <= N; i++)); do
    curl -fsS -o /dev/null -w '%{time_total}\n' "$URL" >>"$tmp" || fail "request $i failed"
done

rc=0
sort -n "$tmp" | awk '
    { a[NR] = $1 }
    END {
        n = NR
        i50 = int(n * 0.50); if (i50 < 1) i50 = 1
        i95 = int(n * 0.95); if (i95 < 1) i95 = 1
        printf "n=%d  p50=%.0fms  p95=%.0fms  max=%.0fms\n", n, a[i50]*1000, a[i95]*1000, a[n]*1000
        exit (a[i95] > 3.0 ? 1 : 0)
    }' || rc=$?
if [[ $rc -eq 0 ]]; then
    ok "P95 within the 3s budget"
else
    warn "P95 above the 3s budget"
fi
