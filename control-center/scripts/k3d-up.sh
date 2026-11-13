#!/usr/bin/env bash
# Build the three images, create the k3d cluster, import the images and roll out the stack.
# The compose stack must be down first: both publish host port 8080.
set -euo pipefail
source "$(dirname "$0")/lib.sh"

CLUSTER="${CC_K3D_CLUSTER:-cc-train0}"

if docker compose ps --quiet cc-web 2>/dev/null | grep -q .; then
    fail "the compose stack is running and holds port 8080; run 'bash scripts/down.sh' first"
fi

log "building images"
docker compose build cc-mgmt cc-pg cc-web

if k3d cluster list --no-headers 2>/dev/null | awk '{print $1}' | grep -qx "$CLUSTER"; then
    log "cluster $CLUSTER already exists"
else
    log "creating cluster $CLUSTER"
    k3d cluster create --config deploy/k3d/cluster.yaml
fi

log "importing images into $CLUSTER"
k3d image import -c "$CLUSTER" cc-pg:17 cc-java:dev cc-web:dev

log "applying manifests"
kubectl apply -k deploy/k3d/k8s

log "waiting for postgres"
kubectl -n cc-train0 rollout status statefulset/cc-pg --timeout=180s

log "waiting for the apps"
kubectl -n cc-train0 rollout status deploy/cc-mgmt --timeout=300s
kubectl -n cc-train0 rollout status deploy/cc-control --timeout=300s
kubectl -n cc-train0 rollout status deploy/cc-web --timeout=120s

log "waiting for the ingress"
wait_http "http://127.0.0.1:8080/api/health" 60 \
    || fail "ingress not reachable; check: kubectl -n cc-train0 get ingress,pods"

ok "k3d stack up: http://127.0.0.1:8080"
kubectl -n cc-train0 get pods,svc,ingress
