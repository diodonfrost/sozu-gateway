# CLAUDE.md

This is the primary agent instruction file for the repository. It is symlinked to `AGENTS.md`
so other coding agents (e.g. OpenAI Codex) pick up the same guidance.

## What this is

A Kubernetes Ingress controller / API gateway built on the [Sōzu](https://github.com/sozu-proxy/sozu)
reverse proxy. The controller watches Kubernetes objects, compiles them into a neutral
intermediate representation (IR), diffs that IR against the last-applied state, and pushes the
minimal set of mutations to a co-located Sōzu instance over its protobuf **command socket** —
hot, with no proxy restarts. **Phase 1 (Ingress + TLS) is complete**; see [PROGRESS.md](PROGRESS.md).

## Commands

```bash
make build          # cargo build --workspace
make test           # cargo test --workspace (unit + golden/snapshot tests)
make lint           # cargo fmt --check + clippy -D warnings (the CI gate)
make fmt            # cargo fmt (write)
make image          # docker build the controller image
make chart-lint     # helm lint + template (also renders with rbac.allowStatusWrites=true)
make e2e            # full in-cluster end-to-end on the current kube-context
```

- **`protoc` is required to build** — `sozu-command-lib`'s `build.rs` runs `prost-build`. The
  devcontainer already installs it; on a bare host `apt-get install protobuf-compiler` first.
- **Run a single test:** `cargo test -p sozu-gw-translator <name>` (crates: `sozu-gw-ir`,
  `sozu-gw-translator`, `sozu-gw-builder`, `sozu-gw-agent`, `sozu-gw-controller`).
- **Snapshot tests use [`insta`](https://insta.rs).** Golden snapshots live in
  `crates/*/tests/snapshots/`. After an intentional change to emitted commands, review/accept
  with `cargo insta review` (or `INSTA_UPDATE=always cargo test`). A diff in a `.snap` is a
  behavior change to scrutinize, not a thing to blindly re-bless.
- The [Makefile](Makefile) is authoritative for task/command names (`image`, `chart-lint`,
  `chart-package`, `e2e`, …); keep the README in sync with it.

## Architecture

```
K8s objects ─▶ reflector caches ─▶ builder ─▶ IR ─▶ translator ─▶ protobuf cmds ─▶ Sōzu socket
```

Five workspace crates, layered so the pure ones can be unit-tested without kube or a socket. The
**purity boundary is load-bearing** — keep `ir`, `builder`, and `translator` free of any I/O
(no `kube` client, no socket); all I/O lives in `sozu-agent` and `controller`.

| Crate | Role | I/O |
|---|---|---|
| [`crates/ir`](crates/ir) | neutral structs (`Cluster`/`Backend`/`Frontend`/`Certificate`/`Ir`) | none |
| [`crates/builder`](crates/builder) | typed K8s objects → IR (+ per-Ingress `Problem`s) | none |
| [`crates/translator`](crates/translator) | pure IR → Sōzu commands, diff vs last-applied | none |
| [`crates/sozu-agent`](crates/sozu-agent) | wrapper around `sozu-command-lib` (socket, ack loop) | **socket** |
| [`crates/controller`](crates/controller) | kube-rs watch/reconcile loop, wires it together | **kube + socket** |

### How the reconcile loop works (`controller/src/main.rs`)

One **singleton, global** reconcile — not per-object. Reflector caches for Ingress, IngressClass,
Service, EndpointSlice, and Secret each ping a single mpsc channel on any change; a debounced
(`SOZU_GW_DEBOUNCE_MS`) reconcile rebuilds the *entire* desired IR from the caches, diffs it
against an in-memory **shadow** (the last successfully-applied `Ir`), and applies only the delta.
A periodic resync (`SOZU_GW_RESYNC_SECS`) self-heals drift.

- The shadow advances **only on a fully successful apply**. On failure it stays put; because every
  emitted request is idempotent, re-diffing from the unchanged shadow converges.
- **Fail-fast philosophy:** if a watch stream ends or caches don't sync within the timeout, the
  process exits so Kubernetes restarts it rather than silently going blind. Never `panic!`.
- Known Phase-1 limitation: restarting *only* the controller container resets the shadow to empty,
  so it re-applies everything (idempotent) but won't prune residual Sōzu state until a later change
  removes it.

### Translator diff strategy — the subtle part

The translator deliberately uses **two different diff strategies**, and changes here are easy to
get wrong:

- **Routing graph** (clusters/backends/frontends): reuse Sōzu's own `ConfigState::diff` so the
  semantics match the data plane exactly. Certificates are kept *out* of this path.
- **Certificates**: diffed by hand, keyed by `(listener, fingerprint)` — Sōzu's own cert identity.
  This (a) emits `ReplaceCertificate` for zero-gap rotation, and (b) sidesteps a `debug_assert` in
  `sozu-command-lib` 2.1.0 that fires when `ConfigState::diff` removes the last cert at a listener.

All output is reordered into **dependency-safe tiers** (`canonicalize` / `tier()`): adds go
clusters → backends → certificates → frontends; removes in reverse; a replacement cert lands
before the old is removed (no TLS gap). This also makes the HashSet-ordered routing diff
deterministic for golden snapshots.

### Conventions that matter

- **Listeners are NOT modelled in the IR.** In Phase 1 the HTTP/HTTPS listeners are declared
  statically in Sōzu's `config.toml` ([deploy/sozu/config.toml](deploy/sozu/config.toml)) and
  activated at boot. The controller only manages clusters/frontends/backends/certificates; their
  listener addresses come from CLI flags (`--http-listener` / `--https-listener`) and must match
  `config.toml`.
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
**0.28** with feature `v1_36` (the e2e cluster's version). Workspace is edition 2021,
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
- `PROGRESS.md` (the dev journal) is in French; code, comments, and docs are in English.
