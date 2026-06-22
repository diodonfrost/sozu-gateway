# sozu-gateway

A Kubernetes **Ingress controller + API gateway** built on the [Sōzu](https://github.com/sozu-proxy/sozu)
reverse proxy, in Rust. The controller watches Kubernetes objects, compiles them into a
neutral intermediate representation (IR), and pushes the resulting state to a co-located
Sōzu instance over its protobuf **command socket** — entirely hot, no restarts.

> **Status: Phase 1 (MVP — Ingress + TLS) in progress.** See [PROGRESS.md](PROGRESS.md)
> and the verified [PROTOCOL.md](PROTOCOL.md).

## Architecture

```
K8s objects ─▶ cache/watch ─▶ Builder ─▶ IR ─▶ Translator ─▶ protobuf cmds ─▶ Sōzu socket
```

Control plane (this repo) and data plane (Sōzu) are **separate processes**; in-cluster
they share the command socket via an `emptyDir` volume in the same Pod.

| Crate | Role | I/O? |
|---|---|---|
| [`crates/ir`](crates/ir) | neutral IR structs (`Listener`/`Cluster`/`Frontend`/`Backend`/`Certificate`) | none |
| [`crates/builder`](crates/builder) | K8s objects → IR (+ status), resolves Service→EndpointSlice & TLS Secrets | none (typed objects in) |
| [`crates/translator`](crates/translator) | pure IR → Sōzu commands, diffs vs last-applied | none |
| [`crates/sozu-agent`](crates/sozu-agent) | thin wrapper around `sozu-command-lib` (socket, send, LoadState) | **socket** |
| [`crates/controller`](crates/controller) | `kube-rs` Controller runtime, wires it all together | **kube + socket** |

`ir`, `builder`, `translator` are kept free of `kube`/socket I/O so they're unit-testable in isolation.

## Key facts (verified)

- `sozu-command-lib` **2.1.0** (LGPL-3.0), Sōzu **2.1.0**, `kube` **4.0**, `k8s-openapi` **0.28** (`v1_36`).
- The Sōzu command socket takes a **bare `Request`** (length-prefixed prost); responses
  come back as `Processing` → `Ok`/`Failure`. Full protocol notes in [PROTOCOL.md](PROTOCOL.md).

## Local development

Prereqs: Rust (stable), Docker, `kubectl`, `helm`. (Sōzu's release binary is musl-linked,
so we run it via the `clevercloud/sozu:2.1.0` image.)

```bash
make build          # cargo build --workspace
make test           # unit + golden tests (Translator, Builder, Agent)
make lint           # cargo fmt --check + clippy -D warnings
make docker-build   # build the controller image
make helm-lint      # helm lint + template
```

## End-to-end on a Kubernetes cluster

`make e2e` (→ [scripts/e2e.sh](scripts/e2e.sh)) runs the whole add-on on your current
kube-context and verifies traffic:

1. builds the controller image and pushes it to an **ephemeral, anonymous registry**
   (`ttl.sh`) so no registry credentials are needed — override with `IMAGE=...` to use
   your own;
2. `helm install`s the add-on (controller + Sōzu in one Pod, `Service type=LoadBalancer`,
   `IngressClass sozu`, RBAC, Sōzu `ConfigMap`);
3. deploys the [demo app](examples/demo-app.yaml) (whoami) + a TLS Secret + an Ingress;
4. curls HTTP and HTTPS through Sōzu (200 + correct SNI cert) and checks that deleting the
   Ingress hot-removes the route (404).

```bash
make e2e                                  # uses ttl.sh
IMAGE=ghcr.io/you/sozu-gw-controller:test make e2e
```

Deploy manually instead:

```bash
helm upgrade --install sozu-gateway deploy/helm -n sozu-system --create-namespace \
  --set image.controller.repository=<your-repo> --set image.controller.tag=<tag>
```

## Architecture validation

The Sōzu command protocol is verified against a live Sōzu, not just read from source — see
[PROTOCOL.md](PROTOCOL.md). Two scratch harnesses reproduce it locally (Sōzu in Docker):

```bash
bash .scratch/run-probe.sh                       # raw protocol probe
RUN_EXAMPLE=agent_smoke bash .scratch/run-probe.sh   # IR -> Translator -> Agent
```
