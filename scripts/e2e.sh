#!/usr/bin/env bash
# End-to-end test: deploy the sozu-gateway add-on + a demo Ingress app on the
# current kube-context and verify HTTP + HTTPS traffic flows through Sōzu.
# Companion suites: e2e-gateway.sh (Gateway API + filters), e2e-l4.sh (raw TCP).
#
# The controller image is pushed to an ephemeral, anonymous registry (ttl.sh) by
# default so this works without registry credentials. Export IMAGE to use your
# own registry (e.g. IMAGE=ghcr.io/you/sozu-gw-controller:test).
set -euo pipefail
source "$(dirname "$0")/e2e-lib.sh"

HOST="app.example.com"

echo "==> context: $(kubectl config current-context)"
ensure_image
ensure_addon
ensure_demo_ns

echo "==> deploy demo app + TLS secret"
kubectl apply -f "$ROOT/examples/ingress/demo-app.yaml"
CERTDIR="$(mktemp -d)"
openssl req -x509 -newkey ec -pkeyopt ec_paramgen_curve:prime256v1 -nodes \
  -keyout "$CERTDIR/tls.key" -out "$CERTDIR/tls.crt" -days 365 \
  -subj "/CN=$HOST" -addext "subjectAltName=DNS:$HOST" 2>/dev/null
kubectl create secret tls app-tls -n "$DEMO_NS" \
  --cert="$CERTDIR/tls.crt" --key="$CERTDIR/tls.key" \
  --dry-run=client -o yaml | kubectl apply -f -
kubectl rollout status deploy/whoami -n "$DEMO_NS" --timeout 120s
sleep 6

echo "--- controller log ---"
kubectl logs -n "$NS" deploy/"$RELEASE" -c controller --tail=20 \
  | grep -E "caches synced|applying changes|problems" || true

pf_start 18080:80 18443:443

echo -n "HTTP  : "; curl -sS -o /tmp/http.out -w "status=%{http_code}\n" -H "Host: $HOST" "http://127.0.0.1:18080/"
echo -n "HTTPS : "; curl -sSk -o /tmp/https.out -w "status=%{http_code}\n" --resolve "$HOST:18443:127.0.0.1" "https://$HOST:18443/"
echo "served cert:"; echo | openssl s_client -connect 127.0.0.1:18443 -servername "$HOST" 2>/dev/null | openssl x509 -noout -subject 2>/dev/null || true
echo "backend says (HTTP body, first lines):"; head -3 /tmp/http.out || true

echo "==> hot update: delete Ingress, expect 404"
kubectl delete ingress whoami -n "$DEMO_NS"
sleep 6
echo -n "HTTP after delete: "; curl -sS -o /dev/null -w "status=%{http_code} (expect 404)\n" -H "Host: $HOST" "http://127.0.0.1:18080/"

echo "==> e2e DONE"
