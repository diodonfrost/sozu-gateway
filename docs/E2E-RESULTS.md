# End-to-end results

This document records the behaviour of the Sōzu gateway as validated end-to-end on a live
Kubernetes cluster, not just in unit tests. The pure logic (builder, translator) is covered by
golden tests in `crates/*/tests/`; this page is about the assembled system serving real traffic.

## Environment

- Managed Kubernetes cluster, **Cilium** CNI (LoadBalancer via Cilium LB-IPAM), single node,
  Kubernetes v1.36.
- Data plane: **Sōzu 2.1.0** (`clevercloud/sozu:2.1.0`), control plane built from this repo.
- The controller image was distributed via the anonymous `ttl.sh` registry (no credentials), the
  add-on installed with the Helm chart, traffic generated with [`hey`](https://github.com/rakyll/hey).

## 1. Functional path (Ingress + TLS)

Installing the chart, then a demo app (`whoami`) with a TLS `Ingress` of class `sozu`:

| Check | Result |
| ----- | ------ |
| `helm install` (controller + Sōzu in one Pod) | Pod `2/2` Running |
| `Service type=LoadBalancer` | external IP assigned (Cilium LB-IPAM) |
| HTTP through Sōzu (`Host: app.example.com`) | **200**, served by a real backend pod |
| HTTPS through Sōzu (SNI `app.example.com`) | **200**, served cert CN = `app.example.com` |
| Controller convergence | reacts to `Secret`/`EndpointSlice` appearing: `0 → 1 → 2` backends, cert added |
| Hot route removal (`kubectl delete ingress`) | subsequent requests `404` — no proxy restart |

## 2. Zero-downtime hot reload (config changes)

A web app (nginx, 3 replicas, `maxUnavailable=0` rolling update, `preStop` drain) behind an
`Ingress`, with **continuous load** (`hey -c 50`) flowing through the LoadBalancer while the app
was churned.

Operations performed during the load window: `rollout restart`, scale `3 → 8`, scale `8 → 1`, an
env-change rollout, and an Ingress **hot-add** of a `/v2` path.

| Metric | Result |
| ------ | ------ |
| Requests | **266 433** over 95 s (~**2 800 req/s**) |
| Status codes | **`[200]` 100 %** — 0 non-200 |
| Transport errors (refused/timeout) | **0** |
| Latency | p50 **17 ms**, p95 23 ms, p99 **29 ms**, max 252 ms |

The controller applied every backend/frontend delta to the **running** Sōzu (no restart): backends
tracked `0→…→8→…→1`, frontends `1→2` on the Ingress edit. **No outage, no 5xx.**

> Zero-downtime during pod churn also relies on the application draining gracefully
> (`maxUnavailable=0` + a `preStop` so a terminating pod keeps serving until the controller has
> reconciled it out of Sōzu). The controller never restarts the proxy and applies only minimal,
> idempotent deltas.

## 3. Data-plane (Sōzu) replacement under load

Replacing the Sōzu Pod itself — mechanically what a Sōzu version bump does — while load was
flowing through the LoadBalancer:

| Scenario | Requests | Result |
| -------- | -------- | ------ |
| `replicaCount=1`, Pod replaced | 128 179 | **`[200]` 100 %**, 0 errors |
| `replicaCount=2`, rolling replace | 170 066 | **`[200]` 100 %**, 0 errors |

Why it held: the rolling update keeps the old, already-programmed Pod serving until the new Pod is
`Ready`, and the new Pod's co-located controller programs Sōzu within the readiness delay — so by
the time the LoadBalancer routes to it, the routes exist.

> **Program gap — now gated.** The controller container exposes `/readyz`, which turns green only
> after its first successful reconcile (Sōzu programmed). A fresh Pod is therefore `Ready` — and
> joins the Service — only once its routes exist, closing the cold-start "program gap" the plain
> Sōzu TCP probe left open. For a robust data-plane upgrade still run `replicaCount >= 2` and set
> `maxUnavailable=0` explicitly. A real version bump must also bump the controller (built against a
> matching `sozu-command-lib`) and the Sōzu image together.

## 4. Gateway API (Phase 2)

Installing the Gateway API CRDs (v1.2.1 standard channel), then a `GatewayClass`, a `Gateway`
(HTTP + HTTPS listeners) and an `HTTPRoute` to the demo app:

| Check | Result |
| ----- | ------ |
| Controller detects the CRDs | logs `Gateway API detected; watching …` (Ingress-only otherwise) |
| Routing through Sōzu (same IR/translator) | HTTP **200** + HTTPS **200** |
| `GatewayClass` status | `Accepted=True` |
| `Gateway` status | `Accepted=True`, `Programmed=True` |
| `HTTPRoute` status (per parent) | `Accepted=True`, `ResolvedRefs=True` |
| Status loop-safety | `HTTPRoute` `resourceVersion` stable over 12 s — no self-triggered loop |

A Gateway route and an Ingress to the same Service share one Sōzu cluster, confirming both APIs
compile to the same IR.

## 5. HTTPRoute filters (Phase 3)

Two HTTPRoutes on the HTTP listener — one carrying header modifiers, one a redirect — exercised
against the `whoami` demo (which echoes the request it received):

| Check | Result |
| ----- | ------ |
| `RequestHeaderModifier` (`set X-Env: prod`) | whoami echoes `X-Env: prod` in the request it sees |
| `ResponseHeaderModifier` (`set X-Served-By: sozu`) | response carries `X-Served-By: sozu` |
| `RequestRedirect` (`scheme: https`, `statusCode: 301`) | **301 Moved Permanently**, `Location: https://redirect.example.com/` |
| Redirect-only route (no `backendRef`) | accepted and programmed as a cluster-less Sōzu frontend |

The redirect route has no `backendRef` (the Gateway API forbids combining `RequestRedirect` with
backends), so it maps to a frontend with no cluster — Sōzu answers the 301 itself.

## Reproduce

```sh
just e2e          # functional path: install + demo app + HTTP/HTTPS checks + hot removal
```

The load/churn harnesses used for sections 2–3 live under `.scratch/` (developer scaffolding, not
shipped): `hot-reload-test2.sh` (config hot reload), `dataplane-upgrade-test.sh` (Sōzu Pod
replacement) and `phase3-e2e.sh` (HTTPRoute filters, section 5).

## Known limitations

- A Sōzu-only restart at runtime (the controller staying up) is not yet detected: the controller's
  in-memory shadow still reflects the pre-restart state, so it won't re-push until the next change.
  Restart the whole Pod, or run `replicaCount >= 2`. (A controller-only restart is handled — the
  shadow is persisted and resumed; see CLAUDE.md.)
