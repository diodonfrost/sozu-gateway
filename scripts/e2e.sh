#!/usr/bin/env bash
# End-to-end test: deploy the sozu-gateway add-on + a demo app on the current
# kube-context and verify HTTP + HTTPS traffic flows through Sōzu.
#
# The controller image is pushed to an ephemeral, anonymous registry (ttl.sh) by
# default so this works without registry credentials. Override IMAGE to use your
# own registry (e.g. IMAGE=ghcr.io/you/sozu-gw-controller:test).
set -euo pipefail

ROOT="$(cd "$(dirname "$0")/.." && pwd)"
cd "$ROOT"

RELEASE="${HELM_RELEASE:-sozu-gateway}"
NS="${HELM_NS:-sozu-system}"
DEMO_NS="sozu-demo"
HOST="app.example.com"
RAND="$(head -c4 /dev/urandom | od -An -tx1 | tr -d ' ')"
IMAGE="${IMAGE:-ttl.sh/sozu-gw-${RAND}:1h}"
REPO="${IMAGE%:*}"
TAG="${IMAGE##*:}"

echo "==> context: $(kubectl config current-context)"
echo "==> image:   $IMAGE"

echo "==> build + push controller image"
docker build -q -t "$IMAGE" . >/dev/null
docker push -q "$IMAGE" >/dev/null 2>&1 || docker push "$IMAGE"

echo "==> helm install add-on"
helm upgrade --install "$RELEASE" charts/sozu-gateway -n "$NS" --create-namespace \
  --set image.controller.repository="$REPO" \
  --set image.controller.tag="$TAG" \
  --set image.controller.pullPolicy=Always \
  --wait --timeout 180s

echo "==> deploy demo app + TLS secret"
kubectl create namespace "$DEMO_NS" --dry-run=client -o yaml | kubectl apply -f -
kubectl apply -f examples/ingress/demo-app.yaml
CERTDIR="$(mktemp -d)"
openssl req -x509 -newkey ec -pkeyopt ec_paramgen_curve:prime256v1 -nodes \
  -keyout "$CERTDIR/tls.key" -out "$CERTDIR/tls.crt" -days 365 \
  -subj "/CN=$HOST" -addext "subjectAltName=DNS:$HOST" 2>/dev/null
kubectl create secret tls app-tls -n "$DEMO_NS" \
  --cert="$CERTDIR/tls.crt" --key="$CERTDIR/tls.key" \
  --dry-run=client -o yaml | kubectl apply -f -
kubectl rollout status deploy/whoami -n "$DEMO_NS" --timeout 120s

echo "==> wait for controller to program Sōzu"
kubectl rollout status deploy/"$RELEASE" -n "$NS" --timeout 120s
sleep 6
echo "--- controller log ---"
kubectl logs -n "$NS" deploy/"$RELEASE" -c controller --tail=20 | grep -E "caches synced|applying changes|problems" || true

echo "==> LoadBalancer status"
kubectl get svc -n "$NS" "$RELEASE" -o wide || true

echo "==> traffic test via port-forward"
kubectl -n "$NS" port-forward "svc/$RELEASE" 18080:80 18443:443 >/tmp/pf.log 2>&1 &
PF=$!
trap 'kill $PF 2>/dev/null' EXIT
sleep 3

echo -n "HTTP  : "; curl -sS -o /tmp/http.out -w "status=%{http_code}\n" -H "Host: $HOST" "http://127.0.0.1:18080/"
echo -n "HTTPS : "; curl -sSk -o /tmp/https.out -w "status=%{http_code}\n" --resolve "$HOST:18443:127.0.0.1" "https://$HOST:18443/"
echo "served cert:"; echo | openssl s_client -connect 127.0.0.1:18443 -servername "$HOST" 2>/dev/null | openssl x509 -noout -subject 2>/dev/null || true
echo "backend says (HTTP body, first lines):"; head -3 /tmp/http.out || true

echo "==> hot update: delete Ingress, expect 404"
kubectl delete ingress whoami -n "$DEMO_NS"
sleep 6
echo -n "HTTP after delete: "; curl -sS -o /dev/null -w "status=%{http_code} (expect 404)\n" -H "Host: $HOST" "http://127.0.0.1:18080/"

echo "==> e2e DONE"
