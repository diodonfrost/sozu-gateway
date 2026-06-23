//! Gateway API (`gateway.networking.k8s.io`) → IR mapping (Phase 2).
//!
//! Gateway API objects compile into the **same** IR as Ingress, reusing the
//! shared cluster/backend resolver, so both APIs converge on one Sōzu state.
//!
//! Phase-2 MVP scope (anything else is reported as a [`Problem`] and skipped, so
//! a feature gap never silently mis-routes):
//!  - `GatewayClass` selected by `controllerName`;
//!  - `Gateway` HTTP/HTTPS listeners mapped to the static `:80`/`:443` listeners
//!    by protocol; HTTPS loads its `certificateRefs` (Terminate only);
//!  - `HTTPRoute` attached by `parentRef` (optional `sectionName`), with path
//!    (`PathPrefix`/`Exact`/`RegularExpression`) and method matches, and exactly
//!    one Service `backendRef` per rule;
//!  - cross-namespace `backendRefs`/`certificateRefs` honour `ReferenceGrant`.
//!
//! Not yet: header/query matches, route filters, weighted multi-backend split,
//! TLS Passthrough (header/query match and weighted split are Sōzu hard limits).

use std::collections::{BTreeMap, BTreeSet};

use serde::Serialize;
use sozu_gw_gateway_api::gateway::GatewayListenersTlsMode;
use sozu_gw_gateway_api::httproute::{
    HttpRouteRulesMatchesMethod, HttpRouteRulesMatchesPath, HttpRouteRulesMatchesPathType,
};
use sozu_gw_ir as ir;

use crate::{
    add_service_route, extract_cert, meta_nn, BuildConfig, Index, Inputs, PortRef, Problem,
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
}

/// Status of one `HTTPRoute` for a single parent Gateway.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct RouteParentResult {
    pub gateway_namespace: String,
    pub gateway_name: String,
    pub accepted: bool,
    pub resolved_refs: bool,
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
}

#[allow(clippy::too_many_arguments)]
pub(crate) fn build_gateway(
    cfg: &BuildConfig,
    inputs: &Inputs,
    index: &Index,
    clusters: &mut BTreeMap<String, ir::Cluster>,
    backends: &mut BTreeMap<String, ir::Backend>,
    frontends: &mut Vec<ir::Frontend>,
    certificates: &mut Vec<ir::Certificate>,
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
        let mut listeners = Vec::new();

        for l in &gw.spec.listeners {
            match l.protocol.as_str() {
                "HTTP" => listeners.push(ListenerInfo {
                    name: l.name.clone(),
                    hostname: l.hostname.clone(),
                    https: false,
                }),
                "HTTPS" => {
                    if load_listener_certs(cfg, inputs, index, &ns, l, certificates, &mut problems)
                    {
                        listeners.push(ListenerInfo {
                            name: l.name.clone(),
                            hostname: l.hostname.clone(),
                            https: true,
                        });
                    }
                }
                other => problems.push(Problem::UnsupportedProtocol {
                    protocol: other.to_string(),
                }),
            }
        }

        let programmed = !listeners.is_empty();
        gw_listeners.insert((ns.clone(), name.clone()), listeners);
        gateways.push(GatewayResult {
            namespace: ns,
            name,
            accepted: true,
            programmed,
            problems,
        });
    }

    // 3. HTTPRoutes attached to our Gateways.
    let mut routes = Vec::new();
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
            let candidates: Vec<&ListenerInfo> = listeners
                .iter()
                .filter(|l| pref.section_name.as_ref().is_none_or(|sn| sn == &l.name))
                .collect();

            let mut problems = Vec::new();
            let mut resolved_refs = true;
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
                    &mut problems,
                    &mut resolved_refs,
                );
            }

            parents.push(RouteParentResult {
                gateway_namespace: gw_ns,
                gateway_name: pref.name.clone(),
                accepted: true,
                resolved_refs,
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
    certificates: &mut Vec<ir::Certificate>,
    problems: &mut Vec<Problem>,
) -> bool {
    let Some(tls) = &listener.tls else {
        problems.push(Problem::TlsEntryWithoutSecret);
        return false;
    };
    if !matches!(tls.mode, None | Some(GatewayListenersTlsMode::Terminate)) {
        problems.push(Problem::UnsupportedTlsMode {
            mode: "Passthrough".to_string(),
        });
        return false;
    }

    let names = listener
        .hostname
        .clone()
        .map(|h| vec![h])
        .unwrap_or_default();
    let mut loaded = false;
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
                inputs, &secret_ns, "Secret", &cref.name, gateway_ns, "Gateway",
            )
        {
            problems.push(Problem::BackendRefNotPermitted {
                reference: format!("Secret {secret_ns}/{}", cref.name),
            });
            continue;
        }
        match index.secrets.get(&(secret_ns, cref.name.clone())) {
            None => problems.push(Problem::SecretNotFound {
                secret: cref.name.clone(),
            }),
            Some(secret) => match extract_cert(secret) {
                Ok((leaf, chain, key)) => {
                    certificates.push(ir::Certificate {
                        listener: cfg.https_listener,
                        certificate: leaf,
                        chain,
                        key,
                        names: names.clone(),
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
    loaded
}

/// Resolve one HTTPRoute rule into frontends on the candidate listeners.
#[allow(clippy::too_many_arguments)]
fn attach_rule(
    cfg: &BuildConfig,
    inputs: &Inputs,
    index: &Index,
    clusters: &mut BTreeMap<String, ir::Cluster>,
    backends: &mut BTreeMap<String, ir::Backend>,
    frontends: &mut Vec<ir::Frontend>,
    route_ns: &str,
    route_hostnames: Option<&[String]>,
    candidates: &[&ListenerInfo],
    rule: &sozu_gw_gateway_api::httproute::HttpRouteRules,
    problems: &mut Vec<Problem>,
    resolved_refs: &mut bool,
) {
    // backendRefs: exactly one Service backend (Sōzu cannot weight-split).
    let refs: Vec<_> = rule.backend_refs.iter().flatten().collect();
    if refs.is_empty() {
        problems.push(Problem::NoReadyEndpoints {
            service: "<none>".to_string(),
        });
        *resolved_refs = false;
        return;
    }
    if refs.len() > 1 {
        problems.push(Problem::WeightedBackendsUnsupported);
        *resolved_refs = false;
        return;
    }
    let br = refs[0];
    let is_service = br.group.as_deref().unwrap_or("").is_empty()
        && br.kind.as_deref().unwrap_or("Service") == "Service";
    if !is_service {
        problems.push(Problem::NonServiceBackend);
        *resolved_refs = false;
        return;
    }
    let backend_ns = br.namespace.clone().unwrap_or_else(|| route_ns.to_string());
    if backend_ns != route_ns
        && !reference_granted(
            inputs,
            &backend_ns,
            "Service",
            &br.name,
            route_ns,
            "HTTPRoute",
        )
    {
        problems.push(Problem::BackendRefNotPermitted {
            reference: format!("Service {backend_ns}/{}", br.name),
        });
        *resolved_refs = false;
        return;
    }
    let Some(port) = br.port else {
        problems.push(Problem::ServicePortNotFound {
            service: br.name.clone(),
            port: "<unspecified>".to_string(),
        });
        *resolved_refs = false;
        return;
    };

    let cluster_id = match add_service_route(
        index,
        clusters,
        backends,
        &backend_ns,
        &br.name,
        &PortRef::Number(port),
    ) {
        Err(problem) => {
            problems.push(problem);
            *resolved_refs = false;
            return;
        }
        Ok((cluster_id, has_endpoints)) => {
            if !has_endpoints {
                problems.push(Problem::NoReadyEndpoints {
                    service: br.name.clone(),
                });
            }
            cluster_id
        }
    };

    // Route filters are a Phase-3 feature; flag and ignore them.
    if rule.filters.as_ref().is_some_and(|f| !f.is_empty()) {
        problems.push(Problem::FilterUnsupported {
            kind: "httproute rule filter".to_string(),
        });
    }

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
            let hosts = effective_hostnames(route_hostnames, l.hostname.as_deref());
            if hosts.is_empty() {
                problems.push(Problem::HostlessRuleSkipped);
                continue;
            }
            for hostname in hosts {
                frontends.push(ir::Frontend {
                    hostname,
                    path: path.clone(),
                    method: method.clone(),
                    cluster_id: cluster_id.clone(),
                    tls: l.https,
                    listener: if l.https {
                        cfg.https_listener
                    } else {
                        cfg.http_listener
                    },
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

fn hosts_compatible(a: &str, b: &str) -> bool {
    a == b || wildcard_covers(a, b) || wildcard_covers(b, a)
}

/// The hostnames a route serves on a listener: the route's hostnames intersected
/// with the listener's hostname constraint (a missing listener hostname matches
/// any; a route with no hostnames inherits the listener's; both missing is an
/// unsupported catch-all).
fn effective_hostnames(route: Option<&[String]>, listener: Option<&str>) -> Vec<String> {
    match (route, listener) {
        (Some(routes), Some(l)) => routes
            .iter()
            .filter(|h| hosts_compatible(h, l))
            .cloned()
            .collect(),
        (Some(routes), None) => routes.to_vec(),
        (None, Some(l)) => vec![l.to_string()],
        (None, None) => Vec::new(),
    }
}

/// Is a cross-namespace reference allowed by a `ReferenceGrant` in the target
/// namespace?
fn reference_granted(
    inputs: &Inputs,
    to_ns: &str,
    to_kind: &str,
    to_name: &str,
    from_ns: &str,
    from_kind: &str,
) -> bool {
    inputs.reference_grants.iter().any(|grant| {
        let grant_ns = grant.metadata.namespace.as_deref().unwrap_or("default");
        grant_ns == to_ns
            && grant
                .spec
                .from
                .iter()
                .any(|f| f.kind == from_kind && f.namespace == from_ns)
            && grant
                .spec
                .to
                .iter()
                .any(|t| t.kind == to_kind && t.name.as_deref().is_none_or(|n| n == to_name))
    })
}
