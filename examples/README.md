# Examples

Manifests that exercise the Sōzu gateway on a real cluster. They assume the add-on is installed
(`helm ... oci://ghcr.io/clevercloud/sozu-gateway`) and an `IngressClass` named `sozu` exists.

| File | What it shows |
| ---- | ------------- |
| [demo-app.yaml](demo-app.yaml) | A 2-replica `whoami` Deployment + Service + a TLS `Ingress` of class `sozu` (host `app.example.com`, `pathType: Prefix`). |
| [gateway-api.yaml](gateway-api.yaml) | The same app exposed via the **Gateway API**: a `GatewayClass`, a `Gateway` (HTTP + HTTPS listeners) and an `HTTPRoute`. Requires the Gateway API CRDs installed in the cluster. |

## Run it

```sh
# A TLS Secret the Ingress references (self-signed, for testing):
openssl req -x509 -newkey ec -pkeyopt ec_paramgen_curve:prime256v1 -nodes \
  -keyout tls.key -out tls.crt -days 365 \
  -subj '/CN=app.example.com' -addext 'subjectAltName=DNS:app.example.com'
kubectl create namespace sozu-demo
kubectl create secret tls app-tls -n sozu-demo --cert=tls.crt --key=tls.key

kubectl apply -f demo-app.yaml

# Send traffic through the proxy (replace <lb-ip> with the Service external IP):
curl     -H 'Host: app.example.com' http://<lb-ip>/
curl -k --resolve app.example.com:443:<lb-ip> https://app.example.com/
```

A request returns `200` served by a `whoami` pod. Editing the Ingress (add a path, change the
backend) or scaling the Deployment is applied hot, with no proxy restart — see
[../docs/E2E-RESULTS.md](../docs/E2E-RESULTS.md).
