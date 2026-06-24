# Ingress examples

Examples driven by the Kubernetes **Ingress** API (and Service annotations). They
all use the `sozu-demo` namespace (create it first: `kubectl create namespace
sozu-demo`) and the IngressClass `sozu`. Hosts are fictional — reach them with a
`Host` header or `--resolve` against the gateway's LoadBalancer IP.

| File | Shows | How |
| ---- | ----- | --- |
| [demo-app.yaml](demo-app.yaml) | A 2-replica `whoami` behind a **TLS Ingress** | `spec.tls` + a `kubernetes.io/tls` Secret |
| [ssl-redirect.yaml](ssl-redirect.yaml) | Automatic **HTTP→HTTPS** redirect | on by default for TLS hosts; opt out with `sozu.io/ssl-redirect: "false"` |
| [load-balancing-and-sticky.yaml](load-balancing-and-sticky.yaml) | LB algorithm + **sticky sessions** | Service annotations `sozu.io/load-balancing`, `sozu.io/sticky-sessions` |
| [connection-limit.yaml](connection-limit.yaml) | **Per-source-IP connection limit** | Service annotations `sozu.io/max-connections-per-ip`, `sozu.io/retry-after` |
| [l4-tcp.yaml](l4-tcp.yaml) | Raw **TCP (L4)** forwarding | Helm `l4.tcpServices` (see the file header) |
| [external-dns.yaml](external-dns.yaml) | **external-dns** integration | DNS from the Ingress host + the controller-published `.status.loadBalancer` (`rbac.allowStatusWrites=true`) |
| [cert-manager.yaml](cert-manager.yaml) | **cert-manager** automatic TLS | `cert-manager.io/cluster-issuer` annotation + `spec.tls.secretName` |

The Service annotations above are cluster-level, so they apply just the same when
the Service is reached through a Gateway API route.

Each file's header comment carries the exact command to verify it. Apply one with:

```sh
kubectl apply -f examples/ingress/load-balancing-and-sticky.yaml
```

A TLS example needs a Secret (self-signed, for testing):

```sh
openssl req -x509 -newkey ec -pkeyopt ec_paramgen_curve:prime256v1 -nodes \
  -keyout tls.key -out tls.crt -days 365 \
  -subj '/CN=app.example.com' -addext 'subjectAltName=DNS:app.example.com'
kubectl create secret tls app-tls -n sozu-demo --cert=tls.crt --key=tls.key
```
