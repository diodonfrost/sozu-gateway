//! Gateway API (`gateway.networking.k8s.io`) → IR mapping (Phase 2).
//!
//! Gateway API objects compile into the **same** IR as Ingress, reusing the
//! shared cluster/backend resolver, so both APIs converge on one Sōzu state.
//!
//! Scope (anything else is reported as a [`Problem`] and skipped, so a feature
//! gap never silently mis-routes):
//!  - `GatewayClass` selected by `controllerName`;
//!  - `Gateway` HTTP/HTTPS listeners mapped to the static `:80`/`:443` listeners
//!    by protocol (`listener.port` must match the *advertised* gateway port for
//!    the protocol — the Service-exposed port, not the pod bind); HTTPS loads
//!    its `certificateRefs` (Terminate only);
//!  - `HTTPRoute` attached by `parentRef` (optional `sectionName`), with path
//!    (`PathPrefix`/`Exact`/`RegularExpression`) and method matches, and either
//!    one Service `backendRef` or a redirect-only rule (no backend);
//!  - filters (Phase 3): RequestHeaderModifier / ResponseHeaderModifier,
//!    RequestRedirect (scheme + status);
//!  - cross-namespace `backendRefs`/`certificateRefs` honour `ReferenceGrant`.
//!
//! Not yet: header/query matches, weighted multi-backend split, TLS Passthrough,
//! RequestMirror, redirect host/path/port, and URLRewrite (Sōzu's rewrite_host
//! rewrites the *backend authority* — it dials the rewritten host — so a literal
//! Gateway rewrite 408s; header/query match and weighted split are Sōzu hard
//! limits).

use std::collections::{BTreeMap, BTreeSet};

use serde::Serialize;
use sozu_gw_gateway_api::gateway::{
    GatewayListenersAllowedRoutesNamespacesFrom as ApiAllowedFrom, GatewayListenersTlsMode,
};
use sozu_gw_gateway_api::httproute::{
    HttpRouteRulesFilters, HttpRouteRulesFiltersRequestRedirectScheme, HttpRouteRulesFiltersType,
    HttpRouteRulesMatchesMethod, HttpRouteRulesMatchesPath, HttpRouteRulesMatchesPathType,
};
use sozu_gw_ir as ir;

use crate::{
    add_service_route, extract_cert, meta_nn, BuildConfig, FingerprintedCert, FrontendSource,
    Index, Inputs, PortRef, Problem, SourcedFrontend,
};

const GW_GROUP: &str = "gateway.networking.k8s.io";

/// Acceptance of one of our `GatewayClass`es.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct GatewayClassResult {
    pub name: String,
    pub accepted: bool,
}

/// Status of one `Gateway` we own.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct GatewayResult {
    pub namespace: String,
    pub name: String,
    pub accepted: bool,
    pub programmed: bool,
    pub problems: Vec<Problem>,
    /// Per-listener status (one entry per declared listener, in spec order).
    pub listeners: Vec<ListenerStatus>,
}

/// Status of one listener, written to `Gateway.status.listeners[]`.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct ListenerStatus {
    pub name: String,
    /// Route kinds this listener admits (e.g. `["HTTPRoute"]`); empty if none.
    pub supported_kinds: Vec<String>,
    /// Number of routes attached to this listener.
    pub attached_routes: i32,
    pub accepted: bool,
    pub accepted_reason: &'static str,
    pub programmed: bool,
    pub programmed_reason: &'static str,
    pub resolved_refs: bool,
    pub resolved_refs_reason: &'static str,
}

/// Status of one `HTTPRoute` for a single parentRef. The parentRef's
/// `sectionName`/`port` are part of its identity — a route may carry several
/// parentRefs to the same Gateway, each with its own result.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct RouteParentResult {
    pub gateway_namespace: String,
    pub gateway_name: String,
    pub section_name: Option<String>,
    pub port: Option<i32>,
    pub accepted: bool,
    /// Gateway API `Accepted` condition reason (e.g. `Accepted`, `NoMatchingParent`).
    pub accepted_reason: &'static str,
    pub resolved_refs: bool,
    /// Gateway API `ResolvedRefs` condition reason (e.g. `ResolvedRefs`,
    /// `BackendNotFound`, `InvalidKind`, `RefNotPermitted`).
    pub resolved_refs_reason: &'static str,
    pub problems: Vec<Problem>,
}

/// Status of one `HTTPRoute` across all of its parents.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct RouteResult {
    pub namespace: String,
    pub name: String,
    pub parents: Vec<RouteParentResult>,
}

pub(crate) struct GatewayBuildResults {
    pub classes: Vec<GatewayClassResult>,
    pub gateways: Vec<GatewayResult>,
    pub routes: Vec<RouteResult>,
}

/// A listener we accepted on one of our Gateways.
struct ListenerInfo {
    name: String,
    hostname: Option<String>,
    https: bool,
    /// The listener's declared port, matched against a parentRef's optional port.
    port: i32,
    /// Which namespaces this listener admits routes from (`allowedRoutes.namespaces`).
    allow_from: AllowedFrom,
    /// Can HTTPRoutes bind here at all (HTTP/HTTPS protocol)? Routes attach to a
    /// routable listener for status/counting even when it is not `programmed`.
    routable: bool,
    /// Successfully programmed into Sōzu (HTTP, or HTTPS with a loaded cert).
    /// Frontends are only emitted for programmed listeners.
    programmed: bool,
    programmed_reason: &'static str,
    accepted: bool,
    accepted_reason: &'static str,
    resolved_refs: bool,
    resolved_refs_reason: &'static str,
    /// Route kinds this listener admits (`["HTTPRoute"]` or a filtered subset).
    supported_kinds: Vec<String>,
}

/// Does an `allowedRoutes.kinds` entry name HTTPRoute (the only kind we serve)?
fn is_httproute_kind(k: &sozu_gw_gateway_api::gateway::GatewayListenersAllowedRoutesKinds) -> bool {
    k.group.as_deref().unwrap_or(GW_GROUP) == GW_GROUP && k.kind == "HTTPRoute"
}

/// The route kinds a listener admits, and whether every requested kind is
/// supported. `allowedRoutes.kinds` unset → just HTTPRoute; a requested kind we
/// don't serve → dropped from the set and flagged (→ `InvalidRouteKinds`).
fn listener_supported_kinds(
    l: &sozu_gw_gateway_api::gateway::GatewayListeners,
) -> (Vec<String>, bool) {
    match l.allowed_routes.as_ref().and_then(|ar| ar.kinds.as_ref()) {
        Some(kinds) if !kinds.is_empty() => {
            let supported = kinds.iter().any(is_httproute_kind);
            let all_ok = kinds.iter().all(is_httproute_kind);
            let set = if supported {
                vec!["HTTPRoute".to_string()]
            } else {
                vec![]
            };
            (set, all_ok)
        }
        _ => (vec!["HTTPRoute".to_string()], true),
    }
}

/// Build the status of one declared listener (whether or not we can program it).
fn build_listener(
    cfg: &BuildConfig,
    inputs: &Inputs,
    index: &Index,
    gw_ns: &str,
    l: &sozu_gw_gateway_api::gateway::GatewayListeners,
    certificates: &mut Vec<FingerprintedCert>,
    problems: &mut Vec<Problem>,
) -> ListenerInfo {
    let routable = matches!(l.protocol.as_str(), "HTTP" | "HTTPS");
    let mut info = ListenerInfo {
        name: l.name.clone(),
        hostname: l.hostname.clone(),
        https: l.protocol == "HTTPS",
        port: l.port,
        allow_from: AllowedFrom::of(l),
        routable,
        programmed: false,
        programmed_reason: "Programmed",
        accepted: true,
        accepted_reason: "Accepted",
        resolved_refs: true,
        resolved_refs_reason: "ResolvedRefs",
        supported_kinds: vec![],
    };

    // An unevaluable namespace selector fails closed exactly like the other
    // fail-closed listener paths: the listener admits no routes (see
    // `AllowedFrom::admits`), must not read cleanly Programmed, and — like
    // the port-mismatch path — loads none of its certificates into Sōzu
    // (material for a listener that serves nothing has no business there).
    let selector_unsupported = routable && matches!(info.allow_from, AllowedFrom::Selector);
    if selector_unsupported {
        info.programmed = false;
        info.programmed_reason = "Invalid";
        problems.push(Problem::NamespaceSelectorUnsupported {
            listener: l.name.clone(),
        });
    }

    // `listener.port` declares the externally advertised port — what clients
    // connect to on the LoadBalancer Service — NOT the pod-level bind (under
    // the chart defaults the Service maps 80 → 8080 / 443 → 8443, so
    // comparing against the bind would reject every standard port-80/443
    // Gateway). The gateway only serves the configured advertised port per
    // protocol (Sōzu's HTTP(S) listeners are fixed at boot); a mismatch
    // fails closed: programming its routes anyway would silently serve them
    // on a port the Gateway never declared.
    let expected_port = if info.https {
        cfg.gateway_https_port
    } else {
        cfg.gateway_http_port
    };
    if routable && l.port != i32::from(expected_port) {
        info.accepted = false;
        info.accepted_reason = "PortUnavailable";
        info.programmed = false;
        info.programmed_reason = "Invalid";
        problems.push(Problem::ListenerPortMismatch {
            listener: l.name.clone(),
            declared: l.port,
            expected: expected_port,
        });
    } else if !selector_unsupported {
        match l.protocol.as_str() {
            "HTTP" => info.programmed = true,
            "HTTPS" => {
                let (loaded, reason) =
                    load_listener_certs(cfg, inputs, index, gw_ns, l, certificates, problems);
                if loaded {
                    info.programmed = true;
                } else {
                    info.programmed = false;
                    info.programmed_reason = "Invalid";
                    info.resolved_refs = false;
                    info.resolved_refs_reason = reason;
                }
            }
            other => {
                info.accepted = false;
                info.accepted_reason = "UnsupportedProtocol";
                info.programmed = false;
                info.programmed_reason = "Invalid";
                problems.push(Problem::UnsupportedProtocol {
                    protocol: other.to_string(),
                });
            }
        }
    }

    if routable {
        let (kinds, all_ok) = listener_supported_kinds(l);
        info.supported_kinds = kinds;
        if !all_ok {
            info.resolved_refs = false;
            info.resolved_refs_reason = "InvalidRouteKinds";
        }
    }

    info
}

/// A listener's `allowedRoutes.namespaces.from` policy. `Selector` is
/// unsupported — there is no Namespace label index to evaluate it against — so
/// it fails CLOSED: the listener admits no routes at all and the gap is
/// reported ([`Problem::NamespaceSelectorUnsupported`]). Treating it as
/// permissive would silently admit every namespace on a control the Gateway
/// owner set precisely to restrict admission.
#[derive(Clone, Copy)]
enum AllowedFrom {
    Same,
    All,
    Selector,
}

impl AllowedFrom {
    fn of(l: &sozu_gw_gateway_api::gateway::GatewayListeners) -> Self {
        match l
            .allowed_routes
            .as_ref()
            .and_then(|ar| ar.namespaces.as_ref())
            .and_then(|ns| ns.from.as_ref())
        {
            Some(ApiAllowedFrom::All) => AllowedFrom::All,
            Some(ApiAllowedFrom::Selector) => AllowedFrom::Selector,
            _ => AllowedFrom::Same, // unset defaults to Same
        }
    }

    /// Does this listener admit a route from `route_ns` (gateway in `gw_ns`)?
    fn admits(self, route_ns: &str, gw_ns: &str) -> bool {
        match self {
            AllowedFrom::All => true,
            AllowedFrom::Same => route_ns == gw_ns,
            // Unsupported means unsupported: an unevaluable selector admits
            // nothing — not even the Gateway's own namespace (`from: Selector`
            // replaces `Same`, it does not extend it).
            AllowedFrom::Selector => false,
        }
    }
}

#[allow(clippy::too_many_arguments)]
pub(crate) fn build_gateway(
    cfg: &BuildConfig,
    inputs: &Inputs,
    index: &Index,
    clusters: &mut BTreeMap<String, ir::Cluster>,
    backends: &mut BTreeMap<String, ir::Backend>,
    frontends: &mut Vec<SourcedFrontend>,
    certificates: &mut Vec<FingerprintedCert>,
) -> GatewayBuildResults {
    // 1. GatewayClasses we own (controllerName matches).
    let mut classes = Vec::new();
    let mut our_classes: BTreeSet<String> = BTreeSet::new();
    for gc in &inputs.gateway_classes {
        let Some(name) = gc.metadata.name.clone() else {
            continue;
        };
        let accepted = gc.spec.controller_name == cfg.controller_name;
        if accepted {
            our_classes.insert(name.clone());
        }
        classes.push(GatewayClassResult { name, accepted });
    }

    // 2. Gateways of our class -> accepted listeners + loaded certificates.
    let mut gateways = Vec::new();
    let mut gw_listeners: BTreeMap<(String, String), Vec<ListenerInfo>> = BTreeMap::new();
    for gw in &inputs.gateways {
        if !our_classes.contains(&gw.spec.gateway_class_name) {
            continue;
        }
        let (ns, name) = meta_nn(&gw.metadata.namespace, &gw.metadata.name);
        let mut problems = Vec::new();
        // Every declared listener gets a status entry, even unsupported / cert-less
        // ones (a route can still attach to them; they just aren't programmed).
        let listeners: Vec<ListenerInfo> = gw
            .spec
            .listeners
            .iter()
            .map(|l| build_listener(cfg, inputs, index, &ns, l, certificates, &mut problems))
            .collect();

        let programmed = listeners.iter().any(|l| l.programmed);
        gw_listeners.insert((ns.clone(), name.clone()), listeners);
        gateways.push(GatewayResult {
            namespace: ns,
            name,
            accepted: true,
            programmed,
            problems,
            // Filled after route attachment, once attachedRoutes is known.
            listeners: Vec::new(),
        });
    }

    // 3. HTTPRoutes attached to our Gateways. `attached` counts, per listener,
    // the routes bound to it (`Gateway.status.listeners[].attachedRoutes`).
    let mut routes = Vec::new();
    let mut attached: BTreeMap<(String, String, String), i32> = BTreeMap::new();
    for route in &inputs.http_routes {
        let (rns, rname) = meta_nn(&route.metadata.namespace, &route.metadata.name);
        let mut parents = Vec::new();

        for pref in route.spec.parent_refs.iter().flatten() {
            let is_gateway = pref.group.as_deref().unwrap_or(GW_GROUP) == GW_GROUP
                && pref.kind.as_deref().unwrap_or("Gateway") == "Gateway";
            if !is_gateway {
                continue;
            }
            let gw_ns = pref.namespace.clone().unwrap_or_else(|| rns.clone());
            let Some(listeners) = gw_listeners.get(&(gw_ns.clone(), pref.name.clone())) else {
                continue; // not one of our Gateways
            };
            // Listeners the parentRef addresses (by sectionName + port), then
            // narrowed to those that admit the route's namespace.
            let addressable: Vec<&ListenerInfo> = listeners
                .iter()
                .filter(|l| l.routable)
                .filter(|l| pref.section_name.as_ref().is_none_or(|sn| sn == &l.name))
                .filter(|l| pref.port.is_none_or(|p| p == l.port))
                .collect();
            // A non-accepted listener (port-mismatched) can never serve the
            // route, so it is no more of a binding target than one that
            // rejects the namespace: excluding it here keeps the route from
            // reading healthy — and from counting toward attachedRoutes —
            // on a listener that will not carry its traffic.
            let candidates: Vec<&ListenerInfo> = addressable
                .iter()
                .copied()
                .filter(|l| l.accepted)
                .filter(|l| l.allow_from.admits(&rns, &gw_ns))
                .collect();

            let mut problems = Vec::new();
            let mut resolved_refs = true;
            let mut resolved_refs_reason = "ResolvedRefs";
            // No addressable listener -> NoMatchingParent; addressable but none
            // admits this namespace -> NotAllowedByListeners.
            let (accepted, accepted_reason) = if addressable.is_empty() {
                (false, "NoMatchingParent")
            } else if candidates.is_empty() {
                (false, "NotAllowedByListeners")
            } else {
                // Attribute this rule's frontends to the (route, parent) pair
                // so a route-key collision can be reported on its result.
                let source = FrontendSource::HttpRoute {
                    namespace: rns.clone(),
                    name: rname.clone(),
                    gateway_namespace: gw_ns.clone(),
                    gateway_name: pref.name.clone(),
                    section_name: pref.section_name.clone(),
                    port: pref.port,
                };
                for rule in route.spec.rules.iter().flatten() {
                    attach_rule(
                        cfg,
                        inputs,
                        index,
                        clusters,
                        backends,
                        frontends,
                        &rns,
                        route.spec.hostnames.as_deref(),
                        &candidates,
                        rule,
                        &source,
                        &mut problems,
                        &mut resolved_refs,
                        &mut resolved_refs_reason,
                    );
                }
                (true, "Accepted")
            };

            // An accepted route binds to each candidate listener (programmed or
            // not) — count it toward that listener's attachedRoutes.
            if accepted {
                for c in &candidates {
                    *attached
                        .entry((gw_ns.clone(), pref.name.clone(), c.name.clone()))
                        .or_insert(0) += 1;
                }
            }

            parents.push(RouteParentResult {
                gateway_namespace: gw_ns,
                gateway_name: pref.name.clone(),
                section_name: pref.section_name.clone(),
                port: pref.port,
                accepted,
                accepted_reason,
                resolved_refs,
                resolved_refs_reason,
                problems,
            });
        }

        if !parents.is_empty() {
            routes.push(RouteResult {
                namespace: rns,
                name: rname,
                parents,
            });
        }
    }

    // Assemble per-listener status now that attachedRoutes is known.
    for g in &mut gateways {
        let Some(listeners) = gw_listeners.get(&(g.namespace.clone(), g.name.clone())) else {
            continue;
        };
        g.listeners = listeners
            .iter()
            .map(|l| ListenerStatus {
                name: l.name.clone(),
                supported_kinds: l.supported_kinds.clone(),
                attached_routes: attached
                    .get(&(g.namespace.clone(), g.name.clone(), l.name.clone()))
                    .copied()
                    .unwrap_or(0),
                accepted: l.accepted,
                accepted_reason: l.accepted_reason,
                programmed: l.programmed,
                programmed_reason: l.programmed_reason,
                resolved_refs: l.resolved_refs,
                resolved_refs_reason: l.resolved_refs_reason,
            })
            .collect();
    }

    GatewayBuildResults {
        classes,
        gateways,
        routes,
    }
}

/// Load an HTTPS listener's `certificateRefs` (Terminate only). Returns whether
/// at least one certificate was loaded (so the listener can serve TLS).
fn load_listener_certs(
    cfg: &BuildConfig,
    inputs: &Inputs,
    index: &Index,
    gateway_ns: &str,
    listener: &sozu_gw_gateway_api::gateway::GatewayListeners,
    certificates: &mut Vec<FingerprintedCert>,
    problems: &mut Vec<Problem>,
) -> (bool, &'static str) {
    let Some(tls) = &listener.tls else {
        problems.push(Problem::TlsEntryWithoutSecret);
        return (false, "InvalidCertificateRef");
    };
    if !matches!(tls.mode, None | Some(GatewayListenersTlsMode::Terminate)) {
        problems.push(Problem::UnsupportedTlsMode {
            mode: "Passthrough".to_string(),
        });
        return (false, "InvalidCertificateRef");
    }

    let names = listener
        .hostname
        .clone()
        .map(|h| vec![h])
        .unwrap_or_default();
    let mut loaded = false;
    let mut ref_not_permitted = false;
    for cref in tls.certificate_refs.iter().flatten() {
        let is_secret = cref.group.as_deref().unwrap_or("").is_empty()
            && cref.kind.as_deref().unwrap_or("Secret") == "Secret";
        if !is_secret {
            problems.push(Problem::InvalidCertificate {
                secret: cref.name.clone(),
                reason: "unsupported certificateRef kind".to_string(),
            });
            continue;
        }
        let secret_ns = cref
            .namespace
            .clone()
            .unwrap_or_else(|| gateway_ns.to_string());
        if secret_ns != gateway_ns
            && !reference_granted(
                inputs, &secret_ns, "", "Secret", &cref.name, gateway_ns, GW_GROUP, "Gateway",
            )
        {
            problems.push(Problem::BackendRefNotPermitted {
                reference: format!("Secret {secret_ns}/{}", cref.name),
            });
            ref_not_permitted = true;
            continue;
        }
        match index.secrets.get(&(secret_ns, cref.name.clone())) {
            None => problems.push(Problem::SecretNotFound {
                secret: cref.name.clone(),
            }),
            Some(secret) => match extract_cert(secret) {
                Ok((leaf, chain, key, fingerprint)) => {
                    certificates.push(FingerprintedCert {
                        fingerprint,
                        cert: ir::Certificate {
                            listener: cfg.https_listener,
                            certificate: leaf,
                            chain,
                            key,
                            names: names.clone(),
                        },
                    });
                    loaded = true;
                }
                Err(reason) => problems.push(Problem::InvalidCertificate {
                    secret: cref.name.clone(),
                    reason,
                }),
            },
        }
    }
    let reason = if loaded {
        "ResolvedRefs"
    } else if ref_not_permitted {
        // A forbidden cross-namespace certificateRef is the listener's headline
        // failure (Gateway API ListenerReasonRefNotPermitted).
        "RefNotPermitted"
    } else {
        "InvalidCertificateRef"
    };
    (loaded, reason)
}

/// Resolve one HTTPRoute rule into frontends on the candidate listeners.
/// Record a `ResolvedRefs` failure, keeping the first reason seen across a route's
/// rules (the Gateway API reports a single reason per parent).
fn fail_ref(resolved: &mut bool, reason: &mut &'static str, new_reason: &'static str) {
    if *resolved {
        *reason = new_reason;
    }
    *resolved = false;
}

#[allow(clippy::too_many_arguments)]
fn attach_rule(
    cfg: &BuildConfig,
    inputs: &Inputs,
    index: &Index,
    clusters: &mut BTreeMap<String, ir::Cluster>,
    backends: &mut BTreeMap<String, ir::Backend>,
    frontends: &mut Vec<SourcedFrontend>,
    route_ns: &str,
    route_hostnames: Option<&[String]>,
    candidates: &[&ListenerInfo],
    rule: &sozu_gw_gateway_api::httproute::HttpRouteRules,
    source: &FrontendSource,
    problems: &mut Vec<Problem>,
    resolved_refs: &mut bool,
    resolved_refs_reason: &mut &'static str,
) {
    // backendRefs: exactly one Service backend (Sōzu cannot weight-split).
    // Parse the route filters into IR filters (Phase 3). Unsupported filters /
    // sub-fields are reported and skipped, never silently mis-applied.
    let filters = parse_filters(rule.filters.as_deref().unwrap_or(&[]), problems);

    // Resolve the backend. A redirect-only rule has no backendRefs (the Gateway
    // API even forbids combining RequestRedirect with backendRefs), so it yields
    // a frontend with no cluster; otherwise exactly one Service backendRef is
    // required (Sōzu cannot weight-split across clusters).
    let refs: Vec<_> = rule.backend_refs.iter().flatten().collect();
    let cluster_id: Option<String> = if refs.is_empty() {
        if filters.redirect.is_some() {
            None
        } else {
            problems.push(Problem::NoReadyEndpoints {
                service: "<none>".to_string(),
            });
            fail_ref(resolved_refs, resolved_refs_reason, "BackendNotFound");
            return;
        }
    } else if refs.len() > 1 {
        problems.push(Problem::WeightedBackendsUnsupported);
        fail_ref(resolved_refs, resolved_refs_reason, "BackendNotFound");
        return;
    } else {
        let br = refs[0];
        let is_service = br.group.as_deref().unwrap_or("").is_empty()
            && br.kind.as_deref().unwrap_or("Service") == "Service";
        if !is_service {
            problems.push(Problem::NonServiceBackend);
            fail_ref(resolved_refs, resolved_refs_reason, "InvalidKind");
            return;
        }
        let backend_ns = br.namespace.clone().unwrap_or_else(|| route_ns.to_string());
        if backend_ns != route_ns
            && !reference_granted(
                inputs,
                &backend_ns,
                "",
                "Service",
                &br.name,
                route_ns,
                GW_GROUP,
                "HTTPRoute",
            )
        {
            problems.push(Problem::BackendRefNotPermitted {
                reference: format!("Service {backend_ns}/{}", br.name),
            });
            fail_ref(resolved_refs, resolved_refs_reason, "RefNotPermitted");
            return;
        }
        let Some(port) = br.port else {
            problems.push(Problem::ServicePortNotFound {
                service: br.name.clone(),
                port: "<unspecified>".to_string(),
            });
            fail_ref(resolved_refs, resolved_refs_reason, "BackendNotFound");
            return;
        };
        match add_service_route(
            index,
            clusters,
            backends,
            &backend_ns,
            &br.name,
            &PortRef::Number(port),
        ) {
            Err(problem) => {
                problems.push(problem);
                fail_ref(resolved_refs, resolved_refs_reason, "BackendNotFound");
                return;
            }
            Ok((cid, has_endpoints)) => {
                if !has_endpoints {
                    problems.push(Problem::NoReadyEndpoints {
                        service: br.name.clone(),
                    });
                }
                Some(cid)
            }
        }
    };

    // Reduce the rule's matches to (path, method) pairs. No `matches` means
    // "match everything" → prefix "/". Header/query matches are skipped.
    let mut route_matches: Vec<(ir::PathMatch, Option<String>)> = Vec::new();
    match rule.matches.as_ref() {
        None => route_matches.push((ir::PathMatch::Prefix("/".to_string()), None)),
        Some(ms) if ms.is_empty() => {
            route_matches.push((ir::PathMatch::Prefix("/".to_string()), None))
        }
        Some(ms) => {
            for m in ms {
                if m.headers.as_ref().is_some_and(|h| !h.is_empty())
                    || m.query_params.as_ref().is_some_and(|q| !q.is_empty())
                {
                    problems.push(Problem::HeaderOrQueryMatchUnsupported);
                    continue;
                }
                route_matches.push((
                    path_match(m.path.as_ref()),
                    m.method.as_ref().and_then(method_string),
                ));
            }
        }
    }

    for (path, method) in &route_matches {
        for l in candidates {
            // A bound-but-unprogrammed listener (e.g. a cert-less HTTPS listener)
            // counts toward attachedRoutes but carries no frontend.
            if !l.programmed {
                continue;
            }
            let hosts = effective_hostnames(route_hostnames, l.hostname.as_deref());
            if hosts.is_empty() {
                // The route's hostnames don't intersect this listener's hostname:
                // the route attaches on a different listener, not a problem.
                continue;
            }
            for hostname in hosts {
                frontends.push(SourcedFrontend {
                    frontend: ir::Frontend {
                        hostname,
                        path: path.clone(),
                        method: method.clone(),
                        cluster_id: cluster_id.clone(),
                        tls: l.https,
                        filters: filters.clone(),
                        listener: if l.https {
                            cfg.https_listener
                        } else {
                            cfg.http_listener
                        },
                    },
                    source: source.clone(),
                });
            }
        }
    }
}

fn path_match(path: Option<&HttpRouteRulesMatchesPath>) -> ir::PathMatch {
    let Some(path) = path else {
        return ir::PathMatch::Prefix("/".to_string());
    };
    let value = path
        .value
        .clone()
        .filter(|v| !v.is_empty())
        .unwrap_or_else(|| "/".to_string());
    match path.r#type {
        Some(HttpRouteRulesMatchesPathType::Exact) => ir::PathMatch::Exact(value),
        Some(HttpRouteRulesMatchesPathType::RegularExpression) => ir::PathMatch::Regex(value),
        // PathPrefix (the default) or unset.
        _ => ir::PathMatch::Prefix(value),
    }
}

/// Parse HTTPRoute filters into neutral IR filters. Supported: header modifiers
/// (set/add→set, remove→delete) and RequestRedirect (scheme + status).
/// Unsupported filters/sub-fields (incl. URLRewrite) are reported.
fn parse_filters(
    filters: &[HttpRouteRulesFilters],
    problems: &mut Vec<Problem>,
) -> ir::FrontendFilters {
    let mut ff = ir::FrontendFilters::default();
    for filter in filters {
        match &filter.r#type {
            HttpRouteRulesFiltersType::RequestHeaderModifier => {
                if let Some(m) = &filter.request_header_modifier {
                    for s in m.set.iter().flatten() {
                        ff.header_mods.push(ir::HeaderMod {
                            on: ir::HeaderTarget::Request,
                            key: s.name.clone(),
                            value: Some(s.value.clone()),
                        });
                    }
                    // Sōzu has no header "append" — `add` is applied as set.
                    for a in m.add.iter().flatten() {
                        ff.header_mods.push(ir::HeaderMod {
                            on: ir::HeaderTarget::Request,
                            key: a.name.clone(),
                            value: Some(a.value.clone()),
                        });
                    }
                    for r in m.remove.iter().flatten() {
                        ff.header_mods.push(ir::HeaderMod {
                            on: ir::HeaderTarget::Request,
                            key: r.clone(),
                            value: None,
                        });
                    }
                }
            }
            HttpRouteRulesFiltersType::ResponseHeaderModifier => {
                if let Some(m) = &filter.response_header_modifier {
                    for s in m.set.iter().flatten() {
                        ff.header_mods.push(ir::HeaderMod {
                            on: ir::HeaderTarget::Response,
                            key: s.name.clone(),
                            value: Some(s.value.clone()),
                        });
                    }
                    for a in m.add.iter().flatten() {
                        ff.header_mods.push(ir::HeaderMod {
                            on: ir::HeaderTarget::Response,
                            key: a.name.clone(),
                            value: Some(a.value.clone()),
                        });
                    }
                    for r in m.remove.iter().flatten() {
                        ff.header_mods.push(ir::HeaderMod {
                            on: ir::HeaderTarget::Response,
                            key: r.clone(),
                            value: None,
                        });
                    }
                }
            }
            HttpRouteRulesFiltersType::RequestRedirect => {
                if let Some(r) = &filter.request_redirect {
                    let scheme = r.scheme.as_ref().map(|s| match s {
                        HttpRouteRulesFiltersRequestRedirectScheme::Http => ir::Scheme::Http,
                        HttpRouteRulesFiltersRequestRedirectScheme::Https => ir::Scheme::Https,
                    });
                    let status = match r.status_code {
                        Some(301) => ir::RedirectStatus::MovedPermanently,
                        _ => ir::RedirectStatus::Found, // 302 is the Gateway default
                    };
                    if r.hostname.is_some() || r.path.is_some() || r.port.is_some() {
                        problems.push(Problem::FilterUnsupported {
                            kind: "RequestRedirect hostname/path/port".to_string(),
                        });
                    }
                    ff.redirect = Some(ir::Redirect { scheme, status });
                }
            }
            HttpRouteRulesFiltersType::UrlRewrite => {
                // Not wired: Sōzu's rewrite_host/rewrite_path rewrite the *backend
                // authority* (the proxy then dials the rewritten host) and expect
                // regex-capture templates, whereas Gateway URLRewrite rewrites the
                // forwarded Host/path toward the *same* backend. Mapping it
                // literally makes the route 408 (verified end-to-end), so report it
                // rather than emit a broken frontend. The translator keeps an
                // ir::Rewrite mapping, so re-wiring is a one-line change should
                // Sōzu's rewrite semantics be reconciled later.
                problems.push(Problem::FilterUnsupported {
                    kind: "URLRewrite".to_string(),
                });
            }
            HttpRouteRulesFiltersType::RequestMirror | HttpRouteRulesFiltersType::ExtensionRef => {
                problems.push(Problem::FilterUnsupported {
                    kind: format!("{:?}", filter.r#type),
                });
            }
        }
    }
    ff
}

/// The wire spelling of an HTTP method (`GET`, `POST`, …) via its serde rename.
fn method_string(method: &HttpRouteRulesMatchesMethod) -> Option<String> {
    serde_json::to_value(method)
        .ok()
        .and_then(|v| v.as_str().map(str::to_string))
}

/// A `*.example.com` wildcard covers exactly one extra label.
fn wildcard_covers(wildcard: &str, host: &str) -> bool {
    wildcard.strip_prefix("*.").is_some_and(|suffix| {
        host.strip_suffix(suffix)
            .and_then(|prefix| prefix.strip_suffix('.'))
            .is_some_and(|label| !label.is_empty() && !label.contains('.'))
    })
}

/// The intersection of a route hostname and a listener hostname: the MORE
/// SPECIFIC of the two when they are compatible (`None` otherwise). When one
/// side is a wildcard covering the other, the covered — narrower — name *is*
/// the intersection: programming the route's own string when the listener is
/// the narrower side would emit a wildcard frontend and route hostnames the
/// listener never admits (the Gateway API intersects, it doesn't widen).
fn host_intersection(route: &str, listener: &str) -> Option<String> {
    if route == listener || wildcard_covers(listener, route) {
        Some(route.to_string())
    } else if wildcard_covers(route, listener) {
        Some(listener.to_string())
    } else {
        None
    }
}

/// The hostnames a route serves on a listener: the route's hostnames intersected
/// with the listener's hostname constraint (a missing listener hostname matches
/// any; a route with no hostnames inherits the listener's; both missing is a
/// catch-all `*`, which Sōzu routes as `DomainRule::Any`).
fn effective_hostnames(route: Option<&[String]>, listener: Option<&str>) -> Vec<String> {
    match (route, listener) {
        (Some(routes), Some(l)) => routes
            .iter()
            .filter_map(|h| host_intersection(h, l))
            .collect(),
        (Some(routes), None) => routes.to_vec(),
        (None, Some(l)) => vec![l.to_string()],
        (None, None) => vec!["*".to_string()],
    }
}

/// Is a cross-namespace reference allowed by a `ReferenceGrant` in the target
/// namespace?
#[allow(clippy::too_many_arguments)]
fn reference_granted(
    inputs: &Inputs,
    to_ns: &str,
    to_group: &str,
    to_kind: &str,
    to_name: &str,
    from_ns: &str,
    from_group: &str,
    from_kind: &str,
) -> bool {
    inputs.reference_grants.iter().any(|grant| {
        let grant_ns = grant.metadata.namespace.as_deref().unwrap_or("default");
        grant_ns == to_ns
            && grant
                .spec
                .from
                .iter()
                .any(|f| f.group == from_group && f.kind == from_kind && f.namespace == from_ns)
            && grant.spec.to.iter().any(|t| {
                t.group == to_group
                    && t.kind == to_kind
                    && t.name.as_deref().is_none_or(|n| n == to_name)
            })
    })
}

#[cfg(test)]
mod tests {
    use super::{host_intersection, wildcard_covers};

    #[test]
    fn wildcard_covers_exactly_one_extra_label() {
        assert!(wildcard_covers("*.example.com", "a.example.com"));
        assert!(!wildcard_covers("*.example.com", "a.b.example.com"));
        assert!(!wildcard_covers("*.example.com", "example.com"));
        // Not a suffix-string match: `notexample.com` must not count.
        assert!(!wildcard_covers("*.example.com", "a.notexample.com"));
        // Only `*.`-prefixed patterns are wildcards; a bare `*` (the builder's
        // catch-all spelling, not representable as a Gateway hostname) is not.
        assert!(!wildcard_covers("*", "example.com"));
        assert!(!wildcard_covers("a.example.com", "a.example.com"));
    }

    #[test]
    fn host_intersection_picks_the_more_specific_name() {
        // Equal → either.
        assert_eq!(
            host_intersection("app.example.com", "app.example.com").as_deref(),
            Some("app.example.com")
        );
        // Wildcard route × specific listener → the listener (narrower) wins.
        assert_eq!(
            host_intersection("*.example.com", "test.example.com").as_deref(),
            Some("test.example.com")
        );
        // Specific route × wildcard listener → the route (narrower) wins.
        assert_eq!(
            host_intersection("test.example.com", "*.example.com").as_deref(),
            Some("test.example.com")
        );
        // Incompatible → empty intersection.
        assert_eq!(host_intersection("a.example.com", "b.example.com"), None);
        assert_eq!(host_intersection("*.example.com", "example.com"), None);
    }
}
