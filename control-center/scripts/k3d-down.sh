#!/usr/bin/env bash
# Delete the k3d cluster (and therefore the in-cluster Postgres volume).
set -euo pipefail
source "$(dirname "$0")/lib.sh"

CLUSTER="${CC_K3D_CLUSTER:-cc-train0}"

if k3d cluster list --no-headers 2>/dev/null | awk '{print $1}' | grep -qx "$CLUSTER"; then
    log "deleting cluster $CLUSTER"
    k3d cluster delete "$CLUSTER"
else
    warn "cluster $CLUSTER does not exist"
fi

kubectl config delete-context "k3d-$CLUSTER" >/dev/null 2>&1 || true
ok "cluster removed"
