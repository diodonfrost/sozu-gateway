<div align="center">

<h1>sozu-gateway</h1>

<p><em>A Kubernetes Ingress controller &amp; API gateway built on the <a href="https://github.com/sozu-proxy/sozu">Sōzu</a> reverse proxy.</em></p>

</div>

<div align="center">

<a href="https://github.com/CleverCloud/sozu-gateway/actions/workflows/ci.yml"><img src="https://img.shields.io/github/actions/workflow/status/CleverCloud/sozu-gateway/ci.yml?branch=master&style=for-the-badge&logo=github&label=CI"></a>
<a href="LICENSE"><img src="https://img.shields.io/github/license/CleverCloud/sozu-gateway?style=for-the-badge&color=blue"></a>
<a href="https://github.com/CleverCloud/sozu-gateway/releases"><img src="https://img.shields.io/github/v/release/CleverCloud/sozu-gateway?style=for-the-badge&logo=github&label=release"></a>

</div>

<br><br>

**sozu-gateway** manages the [Sōzu](https://github.com/sozu-proxy/sozu) reverse proxy as a Kubernetes
Ingress controller and API gateway. It watches Kubernetes objects, compiles them into a neutral
intermediate representation (IR), and pushes the **minimal** set of mutations to a co-located Sōzu
instance over its command socket — so routes, backends and certificates are applied **hot, with no
proxy restarts**.

Two properties are load-bearing: traffic goes to **pod IPs** (resolved from `EndpointSlice`s, never
the Service ClusterIP), and reconciliation is **idempotent** (a single global reconcile rebuilds the
desired state and applies only the delta). Everything goes through the Kubernetes API and the local
command socket — there is no external dependency and no API token.

---

## Features

**Ingress** and **Gateway API** (`GatewayClass` / `Gateway` / `HTTPRoute` / `ReferenceGrant`, with
`Accepted` / `Programmed` / `ResolvedRefs` status) routing through one shared IR; exact + wildcard
hosts; `Prefix` / `Exact` / regex paths; TLS termination from Secrets with SNI and zero-gap rotation;
pod-IP backends from `EndpointSlice`s; HTTPRoute filters (header edits and redirects); raw TCP / UDP
(L4) forwarding; opt-in Prometheus `/metrics`; and idempotent hot reload with no proxy restart.

> **Note:** Basic auth and per-IP rate limiting exist in Sōzu but have no core Gateway API filter, so
> they are not wired yet. See the full support matrix — supported / planned / not supported, with
> Sōzu's hard limits called out — in **[docs/features.md](docs/features.md)**.

---

## Installation

[Helm](https://helm.sh) v3.x and a cluster that can provision a `Service type=LoadBalancer` are
required. Each release publishes the chart to ghcr.io as an OCI artifact (and the matching controller
image), so no image settings are needed:

```console
helm upgrade --install sozu-gateway \
  oci://ghcr.io/clevercloud/sozu-gateway \
  --version <version> --namespace sozu-system --create-namespace --wait
```

Then expose an application by creating an `Ingress` (or `HTTPRoute`) of class `sozu`:

```console
kubectl apply -f examples/ingress/demo-app.yaml
```

The step-by-step [installation guide](docs/getting-started/installation.md) covers verification,
enabling the Gateway API, installing from source, upgrades and uninstall. Runnable manifests live in
the [examples/](examples/README.md) catalog.

---

## Configuration

The controller is configured entirely through the Helm chart
([values.yaml](charts/sozu-gateway/values.yaml)). The most useful values:

| Value | Default | Description |
| ----- | ------- | ----------- |
| `ingressClass.name` | `sozu` | Name of the created `IngressClass` (and `GatewayClass`) |
| `ingressClass.default` | `false` | Make it the cluster's default `IngressClass` |
| `service.type` | `LoadBalancer` | How the proxy is exposed |
| `sozu.httpPort` / `httpsPort` | `8080` / `8443` | In-pod listener ports (the Service maps 80 / 443 to these) |
| `rbac.allowStatusWrites` | `false` | Publish the gateway's LoadBalancer address into Ingress / Gateway `.status` |
| `metrics.enabled` | `false` | Serve Prometheus `/metrics` (pulled from Sōzu over the socket) |
| `l4.tcpServices` / `udpServices` | `{}` | Map `"<port>": "<ns>/<svc>:<port>"` for raw TCP / UDP forwarding |

A few behaviours worth knowing:

- **IngressClass** — only Ingresses selecting class `sozu` are reconciled (`spec.ingressClassName`, the legacy `kubernetes.io/ingress.class` annotation, or class-less when `ingressClass.default=true`).
- **TLS** — `spec.tls[]` Secrets are served by SNI; a host goes HTTPS-on only once its certificate loads, and rotation is applied in place (`ReplaceCertificate`) with no gap.
- **Metrics** — `metrics.enabled=true` pulls `QueryMetrics` over the socket on each scrape and renders Prometheus text on a dedicated `ClusterIP` Service (best-effort; a socket hiccup returns `503`).
- **Data plane** — the controller and Sōzu run as two containers in one Pod sharing the command socket; HTTP / HTTPS listeners are declared statically in Sōzu's [`config.toml`](deploy/sozu/config.toml).

---

## Docs

- [Feature matrix](docs/features.md) — supported / planned / not supported, with Sōzu's hard limits.
- [Installation guide](docs/getting-started/installation.md) — install, verify, Gateway API, source, upgrade, uninstall.
- [End-to-end results](docs/E2E-RESULTS.md) — what was validated on a live cluster, and how to reproduce it.
- [Examples](examples/README.md) — Ingress + Gateway API manifests (TLS, redirects, L4, header filters, …).
- [PROTOCOL.md](PROTOCOL.md) — the verified Sōzu command-socket wire protocol.

---

## Contributing

Issues and pull requests are welcome — open an [issue](https://github.com/CleverCloud/sozu-gateway/issues)
for bugs or feature requests. Tasks run with [`just`](https://github.com/casey/just); before opening a
PR, please make sure the CI gate is green (`protoc` is required to build — `sozu-command-lib`'s
`build.rs` runs `prost-build`):

```console
just lint   # cargo fmt --check + clippy -D warnings
just test   # unit + golden/snapshot tests
```

---

## License

Licensed under the [Apache License 2.0](LICENSE).

The controller links `sozu-command-lib` (LGPL-3.0) only for the command-socket protocol types, which
the LGPL permits from Apache-2.0 code. Sōzu itself (AGPL-3.0) runs as a separate process reached over
a socket, so its license does not extend to this controller.
