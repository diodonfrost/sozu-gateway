# Sōzu gateway for Clever Cloud

[![Continuous integration](https://github.com/CleverCloud/sozu-gateway/actions/workflows/ci.yml/badge.svg?branch=master)](https://github.com/CleverCloud/sozu-gateway/actions/workflows/ci.yml)

> Manages the [Sōzu](https://github.com/sozu-proxy/sozu) reverse proxy as a Kubernetes Ingress
> controller and API gateway — hot-reconfigured over Sōzu's command socket, with no proxy restarts.

## How it works

The controller watches Kubernetes objects (`Ingress`, `IngressClass`, `Service`, `EndpointSlice`,
`Secret`), compiles them into a neutral intermediate representation (IR), diffs that IR against the
last-applied state, and pushes the **minimal** set of mutations to a co-located Sōzu instance over
its protobuf **command socket**. Routes, backends and certificates are therefore applied **hot** —
Sōzu is never restarted.

Two properties are load-bearing:

- **Traffic goes to pod IPs, not the Service ClusterIP.** The controller resolves each Service to
  its ready `EndpointSlice` endpoints, so Sōzu's own load balancing and health awareness stay
  meaningful and routing survives pod churn.
- **Reconciliation is idempotent.** A single global reconcile rebuilds the whole desired state from
  the informer caches and applies only the delta, so re-running on an unchanged cluster emits
  nothing.

Everything goes through the cluster's Kubernetes API and the local command socket — there is no
external dependency and no API token.

## Status

Phase 1 (Ingress + TLS), Phase 2 (Gateway API) and Phase 3 (HTTPRoute filters) are implemented and
validated end-to-end on a live cluster — see [docs/E2E-RESULTS.md](docs/E2E-RESULTS.md): HTTP/HTTPS
traffic through Sōzu with SNI certificate selection, **zero-downtime hot reload** (266k requests,
0 errors, while rolling-restarting, scaling and editing routes), Gateway API routing with
`Accepted`/`Programmed`/`ResolvedRefs` status reporting, and HTTPRoute header/redirect/rewrite
filters. The project is usable but pre-1.0: APIs and defaults may change. Each release publishes the
controller image and the Helm chart to ghcr.io.

HTTP Basic auth and per-IP rate limiting exist in Sōzu but have no core Gateway API filter, so they
remain unwired — see the [feature matrix](docs/features.md).

## Features

Supported today: **Ingress** and **Gateway API** (`GatewayClass`/`Gateway`/`HTTPRoute`/
`ReferenceGrant` with `Accepted`/`Programmed`/`ResolvedRefs` status) routing through one shared IR;
exact + wildcard hosts; `Prefix`/`Exact`/regex paths; TLS termination from Secrets with SNI and
zero-gap rotation; pod-IP backends from EndpointSlices; HTTPRoute filters (header edits, URL
rewrite, redirects); and idempotent hot reload with no proxy restart. Basic auth and per-IP rate
limiting exist in Sōzu but have no core Gateway API filter, so they are not wired yet.

See **[docs/features.md](docs/features.md)** for the full support matrix (supported / planned /
not supported, with the Sōzu hard limits called out).

## Install

To deploy the gateway you need a running Kubernetes cluster, the `kubectl` command with
cluster-admin access and `helm` v3.x. The step-by-step
[installation guide](docs/getting-started/installation.md) covers verification, upgrades and
uninstall.

> **Note:** The HTTP/HTTPS listeners are declared statically in Sōzu's config and exposed through a
> `Service type=LoadBalancer`. Make sure your cluster can provision a LoadBalancer (or override
> `service.type`).

### From the published charts

Each release publishes the [sozu-gateway](charts/sozu-gateway) chart to ghcr.io as an OCI artifact,
versioned on the release tag without the `v` prefix (release `v0.1.0` → chart version `0.1.0`). No
image settings are needed — the chart pulls the matching published image by default:

```
$ helm upgrade --install sozu-gateway \
    oci://ghcr.io/clevercloud/sozu-gateway \
    --version <version> --namespace sozu-system --create-namespace --wait
```

Then expose an application by creating an `Ingress` of class `sozu` — see the
[examples/](examples/README.md) catalog:

```
$ kubectl apply -f examples/ingress/demo-app.yaml
```

### From source

You need `git`, `rust` (stable), `docker`, `kubectl` and `helm`. First clone the repository:

```
$ git clone https://github.com/CleverCloud/sozu-gateway.git
$ cd sozu-gateway
```

> **Note:** `protoc` is required to build — `sozu-command-lib`'s `build.rs` runs `prost-build`
> (`apt-get install protobuf-compiler`).

#### Build the binary

Tasks are run with [`just`](https://github.com/casey/just):

```
$ just build      # cargo build --workspace -> target/debug/sozu-gw-controller
$ just test       # unit + golden/snapshot tests
$ just lint       # cargo fmt --check + clippy -D warnings (the CI gate)
```

#### Build the docker image

```
$ just IMAGE=<your-registry>/sozu-gateway TAG=v0.1.0 image
$ docker push <your-registry>/sozu-gateway:v0.1.0
```

#### From the helm chart

The [sozu-gateway](charts/sozu-gateway) chart installs the controller + Sōzu (one Pod sharing the
command socket), a `Service type=LoadBalancer`, the `IngressClass`, RBAC and Sōzu's `ConfigMap`.
Point it at the image you pushed:

```
$ helm upgrade --install sozu-gateway charts/sozu-gateway \
    --namespace sozu-system --create-namespace \
    --set image.controller.repository=<your-registry>/sozu-gateway \
    --set image.controller.tag=v0.1.0 \
    --wait
```

To exercise the whole stack on the current kube-context — build, install, deploy a demo app and
verify HTTP/HTTPS traffic — run `just e2e` (it uses the anonymous `ttl.sh` registry by default, so
no credentials are needed).

## Credentials

None. The controller talks only to the in-cluster Kubernetes API (via its ServiceAccount) and to
the local Sōzu command socket. No external API token is required, and Sōzu's image is pulled from a
public registry.

## Configuration

### Global

The controller is configured through environment variables, all set by the Helm chart from its
[values](charts/sozu-gateway/values.yaml):

| Name                    | Kind      | Default               | Description                                                        |
| ----------------------- | --------- | --------------------- | ------------------------------------------------------------------ |
| `SOZU_GW_CLASS`         | `String`  | `sozu`                | The `IngressClass` name the controller owns                        |
| `SOZU_GW_SOCKET`        | `String`  | `/run/sozu/sozu.sock` | Path to the Sōzu command socket (shared `emptyDir`)                |
| `SOZU_GW_HTTP_LISTENER` | `Address` | `0.0.0.0:8080`        | HTTP listener address; **must match** Sōzu's `config.toml`         |
| `SOZU_GW_HTTPS_LISTENER`| `Address` | `0.0.0.0:8443`        | HTTPS listener address; **must match** Sōzu's `config.toml`        |
| `SOZU_GW_DEBOUNCE_MS`   | `Integer` | `500`                 | Coalesce bursts of watch events before reconciling                 |
| `SOZU_GW_RESYNC_SECS`   | `Integer` | `60`                  | Periodic full resync interval (self-heals drift)                   |
| `RUST_LOG`              | `String`  | `info`                | Log filter, e.g. `info,sozu_gw_controller=debug`                   |

The most useful Helm values (see [values.yaml](charts/sozu-gateway/values.yaml) for the full list):

| Value                          | Default                            | Description                                            |
| ------------------------------ | ---------------------------------- | ------------------------------------------------------ |
| `replicaCount`                 | `1`                                | Controller + Sōzu Pod replicas                         |
| `ingressClass.name`            | `sozu`                             | Name of the created `IngressClass`                     |
| `ingressClass.default`         | `false`                            | Make it the cluster's default `IngressClass`           |
| `sozu.httpPort` / `httpsPort`  | `8080` / `8443`                    | In-pod listener ports (Service maps 80/443 to these)   |
| `sozu.workerCount`             | `2`                                | Sōzu worker processes                                  |
| `service.type`                 | `LoadBalancer`                     | How the proxy is exposed                               |
| `rbac.allowStatusWrites`       | `false`                            | Grant `ingresses/status` + `events` (Phase 2)          |

### IngressClass

The controller only reconciles Ingresses that select its class, by `spec.ingressClassName`, by the
legacy `kubernetes.io/ingress.class` annotation, or — if `ingressClass.default=true` — Ingresses
with no class set.

### TLS

For each `spec.tls[]` entry the controller loads the referenced `Secret` (`tls.crt` / `tls.key`)
into Sōzu and serves it by SNI. A host becomes HTTPS-enabled only once its certificate has loaded
successfully; a wildcard cert host (`*.example.com`) covers exactly one extra label. Certificate
rotation is applied in place (`ReplaceCertificate`), with no TLS gap.

### Exposing an application

```yaml
apiVersion: networking.k8s.io/v1
kind: Ingress
metadata:
  name: web
spec:
  ingressClassName: sozu
  tls:
    - hosts: ["app.example.com"]
      secretName: app-tls
  rules:
    - host: app.example.com
      http:
        paths:
          - path: /
            pathType: Prefix          # Prefix | Exact | ImplementationSpecific(=regex)
            backend:
              service:
                name: web
                port:
                  number: 80
```

### Data plane (Sōzu)

The control plane (this controller) and the data plane (Sōzu) run as **two containers in one Pod**,
sharing the command socket via an `emptyDir`, both as the same unprivileged uid. The HTTP/HTTPS
listeners are declared statically in Sōzu's `config.toml`
([deploy/sozu/config.toml](deploy/sozu/config.toml), rendered by the chart into a `ConfigMap`); the
controller manages only clusters, frontends, backends and certificates over the socket.

## License

Licensed under the [Apache License 2.0](LICENSE).

The controller links `sozu-command-lib` (LGPL-3.0) only for the command-socket
protocol types; the LGPL permits this from Apache-2.0 code. Sōzu itself (AGPL-3.0)
runs as a separate process reached over a socket, so its license does not extend
to this controller.

## Getting in touch

- Open an [issue](https://github.com/CleverCloud/sozu-gateway/issues) for bugs or feature requests.
