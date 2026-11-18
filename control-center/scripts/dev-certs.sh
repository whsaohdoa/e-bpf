#!/usr/bin/env bash
# Generate the local development PKI for the agent gRPC control plane (T3.1).
#
# The control plane terminates mTLS itself, so compose needs a server certificate
# chain, its private key and the CA that signs agent certificates. This scripts
# creates those, plus a rogue CA and a client signed by it, so the negative mTLS
# tests have a counterpart that must be rejected.
#
# Everything lands in control-center/.tmp/tls, which is git-ignored. Keys are
# unencrypted and world readable on purpose: this is a local-only fixture, never
# a production PKI. Re-run with --force to rotate.
set -euo pipefail
source "$(dirname "$0")/lib.sh"

REPO_DIR="$(cd "$(dirname "$0")/.." && pwd)"
TLS_DIR="${CC_TLS_DIR:-$REPO_DIR/.tmp/tls}"
# The postgres image ships openssl and is pulled for the database anyway; override for air-gapped runs.
CERTS_IMAGE="${CC_CERTS_IMAGE:-${CC_PG_BASE:-postgres:17-bookworm}}"
FORCE=0
[ "${1:-}" = "--force" ] && FORCE=1

if [ "$FORCE" = 0 ] && [ -f "$TLS_DIR/server.crt" ] && [ -f "$TLS_DIR/client.crt" ]; then
  log "TLS material already present in $TLS_DIR (use --force to rotate)"
  exit 0
fi

mkdir -p "$TLS_DIR"
umask 022

log "generating development PKI in $TLS_DIR"

# openssl lives in a container normally. When the script already runs inside an
# image that ships openssl (CI job image, dev container), skip the extra layer.
if [ "${CC_CERTS_IN_DOCKER:-1}" = "0" ] && command -v openssl >/dev/null 2>&1; then
  run_certs() { CERT_OUT_DIR="$TLS_DIR" bash -c "$1"; }
else
  run_certs() { docker run --rm -e CERT_OUT_DIR=/tls -v "$TLS_DIR:/tls" -w /tls --entrypoint /bin/bash "$CERTS_IMAGE" -c "$1"; }
fi

run_certs '
set -euo pipefail
cd "${CERT_OUT_DIR:-/tls}"
rm -f *.key *.crt *.csr *.srl *.ext

# --- CA that signs the control plane server and the agent client -----------------
openssl req -x509 -newkey rsa:2048 -nodes -days 3650 \
  -subj "/CN=cc-train0-dev-ca" -keyout ca.key -out ca.crt >/dev/null 2>&1

# --- server certificate: SAN must match the agent endpoint ----------------------
cat > server.ext <<EOF
basicConstraints = CA:FALSE
keyUsage = digitalSignature, keyEncipherment
extendedKeyUsage = serverAuth
subjectAltName = DNS:localhost, DNS:cc-control, IP:127.0.0.1
EOF
openssl req -newkey rsa:2048 -nodes -subj "/CN=cc-control" \
  -keyout server.key -out server.csr >/dev/null 2>&1
openssl x509 -req -in server.csr -CA ca.crt -CAkey ca.key -CAcreateserial \
  -days 825 -extfile server.ext -out server.crt >/dev/null 2>&1

# --- agent client certificate (CN is the agent id, contract §1) ------------------
cat > client.ext <<EOF
basicConstraints = CA:FALSE
keyUsage = digitalSignature, keyEncipherment
extendedKeyUsage = clientAuth
EOF
openssl req -newkey rsa:2048 -nodes -subj "/CN=agent-m1" \
  -keyout client.key -out client.csr >/dev/null 2>&1
openssl x509 -req -in client.csr -CA ca.crt -CAkey ca.key -CAcreateserial \
  -days 825 -extfile client.ext -out client.crt >/dev/null 2>&1

# --- rogue CA + client: the handshake must fail even though the client cert is valid
openssl req -x509 -newkey rsa:2048 -nodes -days 3650 \
  -subj "/CN=cc-train0-rogue-ca" -keyout rogue-ca.key -out rogue-ca.crt >/dev/null 2>&1
openssl req -newkey rsa:2048 -nodes -subj "/CN=agent-m1" \
  -keyout rogue-client.key -out rogue-client.csr >/dev/null 2>&1
openssl x509 -req -in rogue-client.csr -CA rogue-ca.crt -CAkey rogue-ca.key \
  -CAcreateserial -days 825 -extfile client.ext -out rogue-client.crt >/dev/null 2>&1

rm -f *.csr *.srl server.ext client.ext
chmod 644 *.key *.crt
' >/dev/null

[ -f "$TLS_DIR/server.crt" ] || fail "certificate generation failed (image: $CERTS_IMAGE)"
ok "TLS material ready: $TLS_DIR"
