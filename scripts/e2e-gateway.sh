#!/usr/bin/env bash
# End-to-end test for the Gateway API: routing + the Phase 3 HTTPRoute filters
# (header modifier, request redirect, URL rewrite). Applies the shipped examples
# under examples/api-gateway/, so it also validates those manifests.
set -euo pipefail
source "$(dirname "$0")/e2e-lib.sh"

echo "==> context: $(kubectl config current-context)"
ensure_image
ensure_gateway_api_crds
ensure_addon
ensure_demo_ns

echo "==> apply Gateway API examples (header + redirect filters)"
kubectl apply -f "$ROOT/examples/api-gateway/header-filter.yaml" >/dev/null
kubectl apply -f "$ROOT/examples/api-gateway/redirect.yaml" >/dev/null
kubectl rollout status deploy/echo-headers -n "$DEMO_NS" --timeout 120s
sleep 6

pf_start 18080:80
base="http://127.0.0.1:18080"

echo "==> RequestHeaderModifier: backend sees the injected request header"
body="$(curl -sS -D /tmp/gw-h.out -H 'Host: headers.example.com' "$base/")"
echo "$body" | grep -qi '^X-Env: prod' \
  && echo "  OK   request header X-Env: prod echoed by whoami" \
  || { echo "  FAIL X-Env not seen by backend"; exit 1; }

echo "==> ResponseHeaderModifier: client sees the injected response header"
grep -qi '^X-Served-By: sozu' /tmp/gw-h.out \
  && echo "  OK   response header X-Served-By: sozu" \
  || { echo "  FAIL X-Served-By missing from response"; exit 1; }

echo "==> URLRewrite: host + full path rewritten before the backend"
rw="$(curl -sS -H 'Host: rewrite.example.com' "$base/anything")"
echo "$rw" | grep -qi '^Host: backend.internal' \
  && echo "  OK   Host rewritten to backend.internal" \
  || { echo "  FAIL Host not rewritten"; exit 1; }
echo "$rw" | grep -q 'GET /v2 ' \
  && echo "  OK   path rewritten to /v2" \
  || { echo "  FAIL path not rewritten to /v2"; exit 1; }

echo "==> RequestRedirect: HTTP -> HTTPS 301 (backend-less route)"
curl -sS -o /dev/null -D /tmp/gw-r.out -H 'Host: old.example.com' "$base/"
code="$(awk 'NR==1{print $2}' /tmp/gw-r.out)"
assert_eq "$code" "301" "redirect status"
grep -qi '^location: https://old.example.com/' /tmp/gw-r.out \
  && echo "  OK   Location: https://old.example.com/" \
  || { echo "  FAIL redirect Location wrong"; cat /tmp/gw-r.out; exit 1; }

echo "==> gateway e2e DONE"
