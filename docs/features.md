# Feature support

What the controller does and does not do today (Phase 1 Ingress + TLS, Phase 2 Gateway
API, Phase 3 HTTPRoute filters). It distinguishes what Sōzu
**fundamentally cannot do** from what is simply **not wired up yet**, so a hard constraint is never
mistaken for a roadmap item.

Legend: ✅ supported · 🟡 planned · ❌ not supported.

| Area | Feature | Status | Notes |
| ---- | ------- | :----: | ----- |
| Ingress | IngressClass selection (`spec.ingressClassName`) | ✅ | |
| Ingress | Legacy `kubernetes.io/ingress.class` annotation | ✅ | |
| Ingress | Default IngressClass (`is-default-class`) | ✅ | reconciles class-less Ingresses |
| Ingress | Host match — exact | ✅ | |
| Ingress | Host match — wildcard (`*.example.com`) | ✅ | one extra label |
| Ingress | `pathType: Prefix` | ✅ | |
| Ingress | `pathType: Exact` | ✅ | |
| Ingress | `pathType: ImplementationSpecific` | ✅ | mapped to a Sōzu regex (2.x anchors regexes) |
| Ingress | Multiple Ingresses / hosts / paths | ✅ | de-duplicated by route key |
| Ingress | Rule without a host (catch-all) | ❌ | skipped with a reported problem |
| Ingress | `spec.defaultBackend` | ❌ | not implemented |
| Ingress | `backend.resource` (non-Service backend) | ❌ | only Service backends |
| TLS | Termination from a `Secret` (`tls.crt`/`tls.key`) | ✅ | works with cert-manager-issued Secrets |
| TLS | SNI host selection | ✅ | handled by Sōzu |
| TLS | Wildcard certificate | ✅ | |
| TLS | Zero-gap certificate rotation | ✅ | `ReplaceCertificate` |
| TLS | HTTP → HTTPS redirect | ✅ | automatic for TLS-enabled Ingress hosts (301); opt out with `sozu.io/ssl-redirect: "false"` |
| Routing | Backends = pod IPs from EndpointSlice | ✅ | never the Service ClusterIP |
| Routing | Multi-port Service (match by port name) | ✅ | |
| Routing | Ready-endpoint filtering | ✅ | excludes not-ready endpoints |
| Routing | Hot reload — no proxy restart | ✅ | see [E2E-RESULTS.md](E2E-RESULTS.md) |
| Routing | Idempotent reconcile + periodic resync | ✅ | |
| Routing | Load-balancing algorithm selection | ✅ | Service annotation `sozu.io/load-balancing` (round-robin/random/least-loaded/power-of-two) |
| Routing | Sticky sessions | ✅ | Service annotation `sozu.io/sticky-sessions: "true"` |
| Routing | Per-endpoint weights | 🟡 | IR + translator support it; no standard K8s per-endpoint weight to map from |
| API gateway | Request/response header edits | ✅ | via HTTPRoute `RequestHeaderModifier`/`ResponseHeaderModifier` (Sōzu has no append → `add` applied as set) |
| API gateway | URL rewrite (host + full path) | ✅ | `URLRewrite`; `replacePrefixMatch` not yet |
| API gateway | Redirects (scheme + status) | ✅ | `RequestRedirect` (HTTP→HTTPS, 301/302); host/path/port target not yet |
| API gateway | HTTP Basic auth | 🟡 | Sōzu Cluster field; not wired (no core Gateway filter) |
| API gateway | Connection limit per source IP | ✅ | Service annotation `sozu.io/max-connections-per-ip` (a connection cap, not an RPS quota) |
| API gateway | Match on header value / query param | ❌ | not supported by Sōzu |
| API gateway | Weighted split across multiple Services | ❌ | not supported by Sōzu |
| API gateway | Request mirroring / shadowing | ❌ | not supported by Sōzu |
| Gateway API | `GatewayClass` (by `controllerName`) | ✅ | status `Accepted` reported |
| Gateway API | `Gateway` HTTP/HTTPS listeners | ✅ | mapped to the static `:80`/`:443`; status `Accepted`/`Programmed` |
| Gateway API | `HTTPRoute` (host, path, method) | ✅ | status `Accepted`/`ResolvedRefs` per parent |
| Gateway API | `ReferenceGrant` (cross-namespace refs) | ✅ | gates cross-ns backend/cert refs |
| Gateway API | One Service `backendRef` per rule | ✅ | |
| Gateway API | Weighted multi-`backendRef` split | ❌ | not supported by Sōzu |
| Gateway API | Header/query matches | ❌ | not supported by Sōzu |
| Gateway API | Route filters (header edit, redirect, rewrite) | ✅ | see the API-gateway rows above |
| Gateway API | TLS `Passthrough` | ❌ | terminate only |
| Gateway API | `GRPCRoute` / `TCPRoute` / `TLSRoute` | ❌ | HTTPRoute only |
| Protocols | HTTP / HTTPS (L7) | ✅ | |
| Protocols | TCP / UDP ingress (L4) | ❌ | Sōzu supports it; not wired |
| Operations | Exposure via `Service type=LoadBalancer` | ✅ | |
| Operations | Structured logs (`tracing`) | ✅ | |
| Operations | Gateway API status write-back (loop-safe) | ✅ | Accepted/Programmed/ResolvedRefs |
| Operations | Ingress `status` write-back (loadBalancer) | ✅ | publishes the gateway LB address; enable with `rbac.allowStatusWrites` |
| Operations | Dedicated `/healthz` readiness gate | ✅ | `/readyz` goes green only after the first reconcile, so a Pod takes traffic only once Sōzu is programmed |

## Notes

- **Regex paths (`ImplementationSpecific`).** Sōzu 2.x anchors regexes, so a pattern that matched a
  substring on another controller may need adjusting.
- **API-gateway filters.** Header edits, redirects (scheme + status) and URL rewrites (host +
  full path) are exposed through the IR and Gateway API HTTPRoute filters (Phase 3). Sōzu has no
  header *append*, so a Gateway `add` is applied as a set; redirect host/path/port targets and
  `URLRewrite.replacePrefixMatch` are not expressible yet and are reported rather than half-applied.
  The per-source-IP connection limit is wired through Service annotations (see below). HTTP Basic
  auth exists in Sōzu's data plane but has no core Gateway API filter, so it remains unwired.
- **Hard limits.** Matching on header values or query parameters, weighted traffic split across
  several Services, and request mirroring are not expressible in Sōzu today, so they are out of
  scope rather than merely deferred.

## Annotations

Cluster-level routing is tuned with annotations on the backing **Service** (a cluster is 1:1 with a
Service, so both an Ingress and a Gateway route to that Service share one configuration):

| Annotation | Values | Default | Effect |
| ---------- | ------ | ------- | ------ |
| `sozu.io/load-balancing` | `round-robin`, `random`, `least-loaded`, `power-of-two` | `round-robin` | Sōzu load-balancing algorithm for the cluster. Unknown values fall back to the default. |
| `sozu.io/sticky-sessions` | `"true"` / `"false"` | `"false"` | Pin a client to one backend via a Sōzu sticky cookie. |
| `sozu.io/max-connections-per-ip` | integer | global default | Cap simultaneous connections from one source IP to this cluster. Over the cap → `429`. A non-numeric value is ignored. |
| `sozu.io/retry-after` | integer (seconds) | unset | `Retry-After` header sent on that `429`. |

One annotation is read from the **Ingress** instead (it depends on that Ingress's TLS, not the Service):

| Annotation | Values | Default | Effect |
| ---------- | ------ | ------- | ------ |
| `sozu.io/ssl-redirect` | `"true"` / `"false"` | `"true"` | Redirect HTTP→HTTPS (`301`) for hosts that have a loaded cert. Auto-on; set `"false"` to keep serving plain HTTP. (Gateway API uses an explicit `RequestRedirect` filter instead.) |
