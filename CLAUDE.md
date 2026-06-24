# CLAUDE.md

This is the primary agent instruction file for the repository. It is symlinked to `AGENTS.md`
so other coding agents (e.g. OpenAI Codex) pick up the same guidance.

## What this is

A Kubernetes Ingress controller / API gateway built on the [Sōzu](https://github.com/sozu-proxy/sozu)
reverse proxy. The controller watches Kubernetes objects, compiles them into a neutral
intermediate representation (IR), diffs that IR against the last-applied state, and pushes the
minimal set of mutations to a co-located Sōzu instance over its protobuf **command socket** —
hot, with no proxy restarts. **Phase 1 (Ingress + TLS), Phase 2 (Gateway API) and Phase 3
(HTTPRoute filters: header edits, redirect, URL rewrite) are implemented** and validated
end-to-end; see [docs/E2E-RESULTS.md](docs/E2E-RESULTS.md).

## Commands

Tasks are run with [`just`](https://github.com/casey/just) (`just` with no args lists them):

```bash
just build          # cargo build --workspace
just test           # cargo test --workspace (unit + golden/snapshot tests)
just lint           # cargo fmt --check + clippy -D warnings (the CI gate)
just fmt            # cargo fmt (write)
just image          # docker build the controller image
just chart-lint     # helm lint + template (also renders with rbac.allowStatusWrites=true)
just e2e            # full in-cluster end-to-end on the current kube-context
```

- **`protoc` is required to build** — `sozu-command-lib`'s `build.rs` runs `prost-build`. The
  devcontainer already installs it; on a bare host `apt-get install protobuf-compiler` first.
- **Run a single test:** `cargo test -p sozu-gw-translator <name>` (crates: `sozu-gw-ir`,
  `sozu-gw-translator`, `sozu-gw-builder`, `sozu-gw-agent`, `sozu-gw-controller`).
- **Snapshot tests use [`insta`](https://insta.rs).** Golden snapshots live in
  `crates/*/tests/snapshots/`. After an intentional change to emitted commands, review/accept
  with `cargo insta review` (or `INSTA_UPDATE=always cargo test`). A diff in a `.snap` is a
  behavior change to scrutinize, not a thing to blindly re-bless.
- The [justfile](justfile) is authoritative for task/command names (`image`, `chart-lint`,
  `chart-package`, `e2e`, …); keep the README in sync with it. Override variables before the
  recipe, e.g. `just IMAGE=my/repo TAG=v0.2.0 image`.

## Architecture

```
K8s objects ─▶ reflector caches ─▶ builder ─▶ IR ─▶ translator ─▶ protobuf cmds ─▶ Sōzu socket
```

Six workspace crates, layered so the pure ones can be unit-tested without kube or a socket. The
**purity boundary is load-bearing** — keep `ir`, `builder`, and `translator` free of any socket I/O;
all socket/kube-client I/O lives in `sozu-agent` and `controller`.

| Crate | Role | I/O |
|---|---|---|
| [`crates/ir`](crates/ir) | neutral structs (`Cluster`/`Backend`/`Frontend`/`Certificate`/`Ir`) | none |
| [`crates/gateway-api`](crates/gateway-api) | Gateway API CRD types, kopium-generated (types only) | none |
| [`crates/builder`](crates/builder) | typed Ingress **+ Gateway API** objects → IR (+ `Problem`s/results) | none |
| [`crates/translator`](crates/translator) | pure IR → Sōzu commands, diff vs last-applied | none |
| [`crates/sozu-agent`](crates/sozu-agent) | wrapper around `sozu-command-lib` (socket, ack loop) | **socket** |
| [`crates/controller`](crates/controller) | kube-rs watch/reconcile loop, wires it together | **kube + socket** |

### How the reconcile loop works (`controller/src/main.rs`)

One **singleton, global** reconcile — not per-object. Reflector caches for Ingress, IngressClass,
Service, EndpointSlice, Secret — and, when the Gateway API CRDs are present, GatewayClass, Gateway,
HTTPRoute, ReferenceGrant — each ping a single mpsc channel on any change; a debounced
(`SOZU_GW_DEBOUNCE_MS`) reconcile rebuilds the *entire* desired IR from the caches, diffs it
against an in-memory **shadow** (the last successfully-applied `Ir`), and applies only the delta.
A periodic resync (`SOZU_GW_RESYNC_SECS`) self-heals drift.

- The shadow advances **only on a fully successful apply**. On failure it stays put; because every
  emitted request is idempotent, re-diffing from the unchanged shadow converges.
- **Fail-fast philosophy:** if a watch stream ends or caches don't sync within the timeout, the
  process exits so Kubernetes restarts it rather than silently going blind. Never `panic!`.
- The shadow is **persisted** to the shared volume (`--shadow-file`, default `/run/sozu/shadow.json`)
  on every successful apply and reloaded at startup, so restarting *only* the controller resumes from
  the real baseline and still prunes orphans. It reloads the file **only when Sōzu still holds state**
  (probed via `save_state`): if Sōzu itself restarted (empty), the stale shadow is ignored and the
  full state is re-applied, so a fresh Sōzu is never left unprogrammed.

### Translator diff strategy — the subtle part

The translator deliberately uses **two different diff strategies**, and changes here are easy to
get wrong:

- **Routing graph** (clusters/backends/frontends): reuse Sōzu's own `ConfigState::diff` so the
  semantics match the data plane exactly. Certificates are kept *out* of this path.
- **Certificates**: diffed by hand, keyed by `(listener, fingerprint)` — Sōzu's own cert identity.
  This (a) emits `ReplaceCertificate` for zero-gap rotation, and (b) sidesteps a `debug_assert` in
  `sozu-command-lib` 2.1.0 that fires when `ConfigState::diff` removes the last cert at a listener.

All output is reordered into **dependency-safe tiers** (`canonicalize` / `tier()`): adds go
clusters → backends → certificates → frontends; removes in reverse. Frontend *removes* are
tiered **before** frontend *adds*: Sōzu keys a route by `address;hostname;path[;method]`
(*not* `cluster_id`), so re-pointing a host+path at a different cluster is a `Remove`+`Add` on
the same route key, and adding first would be rejected as a duplicate (`StateError::Exists`)
— there is no atomic frontend replace in 2.1.0. A replacement cert lands before the old is
removed (no TLS gap). This also makes the HashSet-ordered routing diff deterministic for
golden snapshots.

### Gateway API (Phase 2)

`crates/gateway-api` holds **kopium-generated** CRD types (v1.2.1 standard channel,
`--schema=disabled`; regenerate per its README — do not hand-edit). The builder's
[`gateway` module](crates/builder/src/gateway.rs) maps GatewayClass/Gateway/HTTPRoute through the
**same** Service→pod-IP resolver and into the **same** IR as Ingress (a route and an Ingress to one
Service share a cluster). Gateway listeners map to the static `:80`/`:443` by protocol; cross-ns
refs are gated on ReferenceGrant. Anything Sōzu can't represent (weighted multi-backend split,
header/query matches, TLS passthrough) is reported as a `Problem` and skipped, never approximated.

**Phase 3 — HTTPRoute filters.** `RequestHeaderModifier`/`ResponseHeaderModifier`, `RequestRedirect`
(scheme + status) and `URLRewrite` (hostname + `replaceFullPath`) compile into per-frontend
`ir::FrontendFilters`, which the translator maps onto Sōzu's frontend fields. Two honesty rules
hold: Sōzu has no header *append* so a Gateway `add` is applied as a set; and unsupported sub-fields
(redirect host/path/port, `replacePrefixMatch`, `RequestMirror`) are reported, never half-applied. A
`RequestRedirect` rule has no `backendRef` (the API forbids it), so it becomes a **cluster-less
frontend** — hence `ir::Frontend::cluster_id` is `Option<String>`.

The CRDs are **optional**: the controller probes for them and runs Ingress-only if absent. Status
(`Accepted`/`Programmed`/`ResolvedRefs`) is written by [`controller/src/status.rs`](crates/controller/src/status.rs),
which is **loop-safe** — it reuses `lastTransitionTime` for unchanged conditions and skips no-op
patches, so the controller's own status writes never re-trigger it. Status writes are best-effort.

### Conventions that matter

- **HTTP/HTTPS listeners are NOT modelled in the IR.** They are declared
  statically in Sōzu's `config.toml` ([deploy/sozu/config.toml](deploy/sozu/config.toml)) and
  activated at boot; their addresses come from CLI flags (`--http-listener` / `--https-listener`)
  and must match `config.toml`. **L4 (TCP/UDP) listeners are the exception**: their ports are
  user-defined (`tcp/udp-services` ConfigMaps), so the IR carries `ir::L4Frontend`s and the
  translator adds + activates the listeners dynamically over the socket — `ConfigState::diff`
  emits `Add{Tcp,Udp}Listener` + `ActivateListener` (and the reverse on removal) for free.
- **Backends are pod IP:port resolved from EndpointSlices — never the Service ClusterIP.** When a
  Service has multiple ports, match the EndpointSlice port by name; only fall back to the sole port
  when there is exactly one (don't guess `first()`).
- A frontend becomes HTTPS-enabled only if a TLS host with a *successfully loaded* cert covers it.
  Wildcard TLS hosts (`*.example.com`) cover exactly one extra label.
- The Sōzu command socket takes a **bare length-prefixed `Request`**; replies come back as
  `Processing` → `Ok`/`Failure`, so every send loops until a terminal status. This protocol is
  **verified against a live Sōzu**, documented in [PROTOCOL.md](PROTOCOL.md) (the source of truth
  for the translator), with raw research notes in `.scratch/recon/`. Never reimplement the wire
  format or invent protobuf fields — reuse the crate's types and conversions
  (e.g. `addr.into()`, never hand-pack an address).

### Version pins (verified, do not bump casually)

`sozu-command-lib` **2.1.0** (LGPL-3.0) against Sōzu **2.1.0**, `kube` **4**, `k8s-openapi`
**0.28** with feature `v1_36` (the e2e cluster's version). Gateway API types are generated from the
**v1.2.1** standard-channel CRDs with `kopium` **0.24** (the published `gateway-api` crate targets
`kube` 3 / `k8s-openapi` 0.27, so it can't be used here). Workspace is edition 2021,
rust-version 1.88.

## Deployment model

Control plane (this repo) and data plane (Sōzu) are **separate processes/containers in one Pod**,
sharing the command socket via an `emptyDir` volume. Both run as the **same unprivileged uid
(1000)** so they can share that socket. The Helm chart ([charts/sozu-gateway](charts/sozu-gateway))
ships both containers, an `IngressClass`, RBAC, and Sōzu's `ConfigMap`. The Sōzu image is used
as-is (`clevercloud/sozu:2.1.0`) because the release binary is musl-linked.

Releases (`v*` tags) publish the controller image and the Helm chart (OCI) to
`ghcr.io/clevercloud/sozu-gateway` via [.github/workflows/release.yml](.github/workflows/release.yml).

## Working notes

- `.scratch/` is research/probe scaffolding (live-Sōzu protocol probes, recon notes), not part of
  the shipped product.
- Errors: typed per-crate with `thiserror`; `anyhow` only in the controller binary.
- Code, comments, and docs are in English.
