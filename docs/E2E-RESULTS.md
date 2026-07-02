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

## 6. Gateway API conformance (GATEWAY-HTTP, v1.2.1)

The **official** `kubernetes-sigs/gateway-api` v1.2.1 conformance suite, `GATEWAY-HTTP` profile,
run against a live cluster (`GatewayClass=sozu`, `rbac.allowStatusWrites=true`). Report:
[docs/conformance/gateway-http-v1.2.1-report.yaml](conformance/gateway-http-v1.2.1-report.yaml).

| | Passed | Failed | Result |
|---|---|---|---|
| Core | **16** | 17 | failure |
| Extended (declared: `HTTPRouteResponseHeaderModification`, `HTTPRouteSchemeRedirect`, `HTTPRouteMethodMatching`) | 0 | 3 | failure |

The profile is **not passing** — and with Sōzu it **cannot** be (see hard limits below), so the goal
is a well-documented **partial** result, not the "Conformant" badge. Running the suite surfaced and
fixed real bugs (`observedGeneration`; a reconcile wedge on non-idempotent `Remove*`), unblocked
hostname-less routing (catch-all `*`), added invalid-route status reasons + `allowedRoutes.namespaces`,
per-listener status (`.status.listeners[]`), and cert `ReferenceGrant` denial reporting — taking core
from **3 → 16**.

> Counts vary run-to-run **15–16** because a few status-polling tests flake against the poc
> cluster's API latency (raise the conformance client QPS); ~17–18 distinct core tests pass across
> runs/in isolation. **Note:** the hostname/path routing tests (`ListenerHostnameMatching`,
> `HostnameIntersection`, `PathMatchOrder`) were verified to route correctly by hand (exact +
> wildcard + precedence + one-label wildcard depth all behave), but fail in the suite at **base
> setup**: the base `same-namespace-with-https-listener` (HTTPS-only) Gateway intermittently isn't
> `Programmed` in time because the controller reports `SecretNotFound` for the suite's
> programmatically-created `tls-validity-checks-certificate` — a setup-timing gate, not a routing bug.

> **Stale vs. the current tree:** the recorded 16/33 predates the `allowedRoutes.namespaces`
> `from: Selector` fail-closed change. The upstream `HTTPRouteCrossNamespace` and
> `GatewayWithAttachedRoutes` tests attach their routes through `from: Selector` Gateways, so
> those recorded passes were artifacts of the old fail-open bug (Selector admitted every
> namespace); with Selector now admitting nothing, the suite needs a re-run. The report YAML is
> kept unedited as the record of that run.

**Passing (16):** the 3 `*ObservedGenerationBump`; `HTTPRouteSimpleSameNamespace`,
`HTTPRouteExactPathMatching`, `HTTPRouteCrossNamespace`, `HTTPRouteServiceTypes`;
`HTTPRouteInvalidParentRefNotMatchingSectionName`, `HTTPRouteInvalidCrossNamespaceParentRef`;
`GatewayWithAttachedRoutes`, `GatewayModifyListeners`, `GatewayInvalidRouteKind`,
`GatewayInvalidTLSConfiguration`, `GatewaySecretReferenceGrant{AllInNamespace,Specific}`,
`GatewaySecret{Invalid,Missing}ReferenceGrant`.
(`GatewayWithAttachedRoutesWithPort8080` also passes in isolation; it flaked here under client
throttling — run the suite with a raised client QPS, see `CONFORMANCE-HANDOFF.md`.)

**Hard ceiling — not fixable with Sōzu / one LoadBalancer** (these stay failed):
- **No HTTP 500.** Sōzu's answers are 301/400/401/404/408/413/421/429/502/503/504/507; an invalid
  `backendRef` yields 503, but the spec/tests want exactly 500 → the `HTTPRouteInvalid*BackendRef` /
  `*ReferenceGrant` / `…PartiallyInvalid…` traffic checks.
- **No weighted split** (`HTTPRouteWeight`) and **no header/query-value matching**
  (`HTTPRouteHeaderMatching`, parts of `HTTPRouteMatching`).
- **Header `set` appends instead of replacing.** Gateway `set` must overwrite an existing header,
  but the deployed `clevercloud/sozu:2.1.0` data plane appends (observed `original,header-set`) —
  so `HTTPRouteRequestHeaderModifier`/`ResponseHeaderModifier` fail. (The 2.1.0 *command-lib*
  documents set/replace; the running binary doesn't honour it, so this is a data-plane gap pending a
  Sōzu build that replaces.)
- **Catch-all collisions.** Clever Cloud's cluster currently allows **one LoadBalancer**, so all
  Gateways share one Sōzu `:80`/`:443`; two hostname-less routes on the same path collide on key
  `(:8080,*,/path)` (first wins). Per-Gateway addresses would need multiple LBs (unavailable), so
  this is a platform-constrained limit — it drives most of the remaining routing/filter failures
  (`HTTPRouteMatchingAcrossRoutes`, `HTTPRoutePathMatchOrder`, `HostnameIntersection`,
  `ListenerHostnameMatching`, and the extended `RedirectScheme`/`MethodMatching`/header-modifier
  tests). Real users on a shared LB route by hostname (no collision).

**Implementable remaining gaps** (would raise the count):
1. **Per-Gateway HTTPS listener** (`HTTPRouteHTTPSListener`) — multi-listener HTTPS with SNI on the
   shared `:443`; intertwined with the catch-all-collision limit above.

(`GatewaySecret{Invalid,Missing}ReferenceGrant` now pass — cert `ReferenceGrant` denial reports
`RefNotPermitted`, with the grant `group` checked too.)

Reproduce: see the harness + commands in `CONFORMANCE-HANDOFF.md`.

## Reproduce

```sh
just e2e          # section 1: Ingress + TLS — install + demo app + HTTP/HTTPS + hot removal
just e2e-gateway  # sections 4–5: Gateway API routing + header/redirect filters
just e2e-l4       # raw TCP (L4) forwarding through Sōzu
just e2e-all      # all three, sharing one freshly-built image
```

Each suite builds + pushes the controller image to the anonymous `ttl.sh` registry by default (no
credentials needed) and installs the add-on on the current kube-context; the scripts live under
[scripts/](../scripts/).

The load/churn harnesses used for sections 2–3 live under `.scratch/` (developer scaffolding, not
shipped): `hot-reload-test2.sh` (config hot reload) and `dataplane-upgrade-test.sh` (Sōzu Pod
replacement).

## Known limitations

- A Sōzu-only restart at runtime (the controller staying up) is not yet detected: the controller's
  in-memory shadow still reflects the pre-restart state, so it won't re-push until the next change.
  Restart the whole Pod, or run `replicaCount >= 2`. (A controller-only restart is handled — the
  shadow is persisted and resumed; see CLAUDE.md.)
