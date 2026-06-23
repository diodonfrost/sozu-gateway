# Feature support

What the controller does and does not do today (Phase 1 Ingress + TLS, Phase 2 Gateway
API). It distinguishes what Sōzu
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
| TLS | HTTP → HTTPS redirect | 🟡 | Sōzu supports it; not wired yet |
| Routing | Backends = pod IPs from EndpointSlice | ✅ | never the Service ClusterIP |
| Routing | Multi-port Service (match by port name) | ✅ | |
| Routing | Ready-endpoint filtering | ✅ | excludes not-ready endpoints |
| Routing | Hot reload — no proxy restart | ✅ | see [E2E-RESULTS.md](E2E-RESULTS.md) |
| Routing | Idempotent reconcile + periodic resync | ✅ | |
| Routing | Load-balancing algorithm selection | 🟡 | fixed round-robin today |
| Routing | Sticky sessions | 🟡 | Sōzu supports it; not exposed |
| Routing | Per-endpoint weights | 🟡 | IR supports it; equal weights today |
| API gateway | Request/response header edits | 🟡 | Sōzu filter; Phase 3 |
| API gateway | Path / host rewrite | 🟡 | Sōzu filter; Phase 3 |
| API gateway | Redirects | 🟡 | Sōzu filter; Phase 3 |
| API gateway | HTTP Basic auth | 🟡 | Sōzu filter; Phase 3 |
| API gateway | Rate limiting (per source IP) | 🟡 | Sōzu filter; Phase 3 |
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
| Gateway API | Route filters (header edit, redirect, rewrite…) | 🟡 | Phase 3 (Sōzu data-plane ready) |
| Gateway API | TLS `Passthrough` | ❌ | terminate only |
| Gateway API | `GRPCRoute` / `TCPRoute` / `TLSRoute` | ❌ | HTTPRoute only |
| Protocols | HTTP / HTTPS (L7) | ✅ | |
| Protocols | TCP / UDP ingress (L4) | ❌ | Sōzu supports it; not wired |
| Operations | Exposure via `Service type=LoadBalancer` | ✅ | |
| Operations | Structured logs (`tracing`) | ✅ | |
| Operations | Gateway API status write-back (loop-safe) | ✅ | Accepted/Programmed/ResolvedRefs |
| Operations | Ingress `status` write-back (loadBalancer) | 🟡 | `rbac.allowStatusWrites` |
| Operations | Dedicated `/healthz` readiness gate | ❌ | not yet (see e2e caveats) |

## Notes

- **Regex paths (`ImplementationSpecific`).** Sōzu 2.x anchors regexes, so a pattern that matched a
  substring on another controller may need adjusting.
- **API-gateway filters.** Header edits, rewrites, redirects, Basic auth and per-IP rate limiting
  already exist in Sōzu's data plane; Phase 3 will expose them through the IR (and Gateway API
  filters / annotations).
- **Hard limits.** Matching on header values or query parameters, weighted traffic split across
  several Services, and request mirroring are not expressible in Sōzu today, so they are out of
  scope rather than merely deferred.
