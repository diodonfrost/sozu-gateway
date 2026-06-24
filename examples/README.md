# Examples

Manifests that exercise the Sōzu gateway on a real cluster. They assume the
add-on is installed (`helm ... oci://ghcr.io/clevercloud/sozu-gateway`) and that
an `IngressClass` / `GatewayClass` named `sozu` exists. All examples use the
`sozu-demo` namespace — create it first with `kubectl create namespace sozu-demo`
(the manifests no longer bundle the Namespace, so deleting one never tears down
the shared namespace).

- **[ingress/](ingress/)** — the Kubernetes **Ingress** API and Service
  annotations: TLS, automatic HTTP→HTTPS redirect, load-balancing algorithm &
  sticky sessions, per-IP connection limit, and raw TCP (L4) forwarding.
- **[api-gateway/](api-gateway/)** — the **Gateway API**
  (`GatewayClass`/`Gateway`/`HTTPRoute`): routing plus header/redirect/rewrite
  filters. Requires the Gateway API CRDs.

Send traffic through the proxy using the Service's external IP (or a
port-forward), with a `Host` header or `--resolve` for the fictional hostnames:

```sh
curl     -H 'Host: app.example.com' http://<lb-ip>/
curl -k --resolve app.example.com:443:<lb-ip> https://app.example.com/
```

Editing a route or scaling a Deployment is applied hot, with no proxy restart —
see [../docs/E2E-RESULTS.md](../docs/E2E-RESULTS.md).
