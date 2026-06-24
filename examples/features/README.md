# Feature examples

One minimal, self-contained manifest per feature. They all live in the
`sozu-examples` namespace with distinct names, so you can apply any one on its
own — or all of them together. Hosts are fictional: reach them with a `Host`
header or `--resolve` against the gateway's LoadBalancer IP.

Prerequisites: the add-on installed, an `IngressClass`/`GatewayClass` named
`sozu`, and (for the Gateway API files) the Gateway API CRDs.

| File | Feature | How it's configured |
| ---- | ------- | ------------------- |
| [load-balancing-and-sticky.yaml](load-balancing-and-sticky.yaml) | Load-balancing algorithm + sticky sessions | Service annotations `sozu.io/load-balancing`, `sozu.io/sticky-sessions` |
| [connection-limit.yaml](connection-limit.yaml) | Per-source-IP connection limit | Service annotations `sozu.io/max-connections-per-ip`, `sozu.io/retry-after` |
| [ssl-redirect.yaml](ssl-redirect.yaml) | Automatic HTTP→HTTPS redirect | TLS Ingress (auto); opt out with `sozu.io/ssl-redirect: "false"` |
| [header-filter.yaml](header-filter.yaml) | Request/response header edits | HTTPRoute `RequestHeaderModifier` / `ResponseHeaderModifier` |
| [url-rewrite.yaml](url-rewrite.yaml) | URL rewrite (host + full path) | HTTPRoute `URLRewrite` |
| [redirect.yaml](redirect.yaml) | Redirect (scheme + status), backendless route | HTTPRoute `RequestRedirect` |

Each file's header comment carries the exact `curl` to verify it. Apply one with:

```sh
kubectl apply -f examples/features/header-filter.yaml
```
