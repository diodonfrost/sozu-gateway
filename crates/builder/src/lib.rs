//! Builder: Kubernetes objects → IR.
//!
//! Pure and I/O-free: it operates on already-fetched typed objects (the
//! controller fills [`Inputs`] from its reflector caches), resolves references
//! (Service → EndpointSlice pod IPs, TLS Secret → certificate), validates, and
//! produces the [`ir::Ir`] plus a per-Ingress [`IngressResult`] for status
//! reporting. No `kube` client, no socket — so it is fully unit-testable.
#![forbid(unsafe_code)]

use std::collections::{BTreeMap, BTreeSet};
use std::net::{IpAddr, SocketAddr};

use k8s_openapi::api::core::v1::{Secret, Service};
use k8s_openapi::api::discovery::v1::EndpointSlice;
use k8s_openapi::api::networking::v1::Ingress;
use serde::Serialize;
use sozu_gw_gateway_api::{Gateway, GatewayClass, HttpRoute, ReferenceGrant};
use sozu_gw_ir as ir;

mod gateway;
pub use gateway::{GatewayClassResult, GatewayResult, RouteParentResult, RouteResult};

const SERVICE_NAME_LABEL: &str = "kubernetes.io/service-name";
const LEGACY_CLASS_ANNOTATION: &str = "kubernetes.io/ingress.class";

/// Listener addresses and class identity the build is parameterised over.
#[derive(Debug, Clone)]
pub struct BuildConfig {
    pub class_name: String,
    pub class_is_default: bool,
    /// GatewayClass `controllerName` we own (Gateway API).
    pub controller_name: String,
    pub http_listener: SocketAddr,
    pub https_listener: SocketAddr,
}

impl Default for BuildConfig {
    fn default() -> Self {
        Self {
            class_name: "sozu".to_string(),
            class_is_default: false,
            controller_name: "sozu.io/gateway-controller".to_string(),
            http_listener: "0.0.0.0:80".parse().expect("const addr"),
            https_listener: "0.0.0.0:443".parse().expect("const addr"),
        }
    }
}

/// Already-fetched cluster objects. The controller pushes everything it watches;
/// `build` indexes and resolves. Order-independent.
#[derive(Default)]
pub struct Inputs {
    pub ingresses: Vec<Ingress>,
    pub services: Vec<Service>,
    pub endpointslices: Vec<EndpointSlice>,
    pub secrets: Vec<Secret>,
    // Gateway API (Phase 2). Empty when only Ingress is in use.
    pub gateway_classes: Vec<GatewayClass>,
    pub gateways: Vec<Gateway>,
    pub http_routes: Vec<HttpRoute>,
    pub reference_grants: Vec<ReferenceGrant>,
}

/// A problem found while building one Ingress. Surfaced for status + logging;
/// non-fatal problems still let the rest of the Ingress translate.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub enum Problem {
    SecretNotFound { secret: String },
    TlsEntryWithoutSecret,
    InvalidCertificate { secret: String, reason: String },
    NonServiceBackend,
    ServiceNotFound { service: String },
    ServicePortNotFound { service: String, port: String },
    NoReadyEndpoints { service: String },
    HostlessRuleSkipped,
    // Gateway API (Phase 2) — features Sōzu or this phase does not cover yet.
    UnsupportedTlsMode { mode: String },
    UnsupportedProtocol { protocol: String },
    WeightedBackendsUnsupported,
    HeaderOrQueryMatchUnsupported,
    FilterUnsupported { kind: String },
    BackendRefNotPermitted { reference: String },
}

/// Per-Ingress build result for status reporting.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct IngressResult {
    pub namespace: String,
    pub name: String,
    pub accepted: bool,
    pub problems: Vec<Problem>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct BuildOutput {
    pub ir: ir::Ir,
    /// Per-Ingress results (status/logging).
    pub results: Vec<IngressResult>,
    /// Gateway API results (Phase 2).
    pub gateway_classes: Vec<GatewayClassResult>,
    pub gateways: Vec<GatewayResult>,
    pub routes: Vec<RouteResult>,
}

/// A reference to a Service port from an Ingress backend: by number or by name.
pub(crate) enum PortRef {
    Number(i32),
    Name(String),
}

// ----------------------------------------------------------------------------

pub(crate) fn meta_nn(namespace: &Option<String>, name: &Option<String>) -> (String, String) {
    (
        namespace.clone().unwrap_or_else(|| "default".to_string()),
        name.clone().unwrap_or_default(),
    )
}

/// Does this Ingress belong to our IngressClass?
pub fn is_ours(ingress: &Ingress, cfg: &BuildConfig) -> bool {
    if let Some(spec) = &ingress.spec {
        if let Some(class) = &spec.ingress_class_name {
            return class == &cfg.class_name;
        }
    }
    if let Some(annotations) = &ingress.metadata.annotations {
        if let Some(class) = annotations.get(LEGACY_CLASS_ANNOTATION) {
            return class == &cfg.class_name;
        }
    }
    // No class set anywhere: ours only if our IngressClass is the cluster default.
    cfg.class_is_default
}

/// Map a Kubernetes `pathType` + `path` to an IR path match.
fn path_match(path_type: &str, path: Option<&str>) -> ir::PathMatch {
    let value = match path {
        Some(p) if !p.is_empty() => p.to_string(),
        _ => "/".to_string(),
    };
    match path_type {
        "Exact" => ir::PathMatch::Exact(value),
        "ImplementationSpecific" => ir::PathMatch::Regex(value),
        // "Prefix" and anything unknown default to Prefix (the safest superset).
        _ => ir::PathMatch::Prefix(value),
    }
}

/// Does a set of TLS hosts (possibly wildcards) cover `host`?
fn tls_covers(tls_hosts: &BTreeSet<String>, host: &str) -> bool {
    if tls_hosts.contains(host) {
        return true;
    }
    // A `*.example.com` wildcard covers exactly one extra label: `a.example.com`
    // but NOT `a.b.example.com` and not the bare apex `example.com`.
    tls_hosts.iter().any(|pattern| {
        pattern.strip_prefix("*.").is_some_and(|suffix| {
            host.strip_suffix(suffix)
                .and_then(|prefix| prefix.strip_suffix('.'))
                .is_some_and(|label| !label.is_empty() && !label.contains('.'))
        })
    })
}

/// Extract leaf + chain + key PEM from a TLS Secret (`tls.crt` / `tls.key`).
pub(crate) fn extract_cert(secret: &Secret) -> Result<(String, Vec<String>, String), String> {
    let data = secret
        .data
        .as_ref()
        .ok_or_else(|| "secret has no data".to_string())?;
    let crt = data
        .get("tls.crt")
        .ok_or_else(|| "missing tls.crt".to_string())?;
    let key = data
        .get("tls.key")
        .ok_or_else(|| "missing tls.key".to_string())?;
    let crt = String::from_utf8(crt.0.clone()).map_err(|_| "tls.crt is not UTF-8".to_string())?;
    let key = String::from_utf8(key.0.clone()).map_err(|_| "tls.key is not UTF-8".to_string())?;

    let mut chain = sozu_command_lib::certificate::split_certificate_chain(crt);
    if chain.is_empty() {
        return Err("tls.crt contains no PEM blocks".to_string());
    }
    let leaf = chain.remove(0);
    Ok((leaf, chain, key))
}

// ----------------------------------------------------------------------------

pub(crate) struct Index<'a> {
    pub(crate) services: BTreeMap<(String, String), &'a Service>,
    pub(crate) secrets: BTreeMap<(String, String), &'a Secret>,
    /// (namespace, service-name) -> slices
    pub(crate) slices: BTreeMap<(String, String), Vec<&'a EndpointSlice>>,
}

impl<'a> Index<'a> {
    pub(crate) fn build(inputs: &'a Inputs) -> Self {
        let mut services = BTreeMap::new();
        for svc in &inputs.services {
            services.insert(meta_nn(&svc.metadata.namespace, &svc.metadata.name), svc);
        }
        let mut secrets = BTreeMap::new();
        for secret in &inputs.secrets {
            secrets.insert(
                meta_nn(&secret.metadata.namespace, &secret.metadata.name),
                secret,
            );
        }
        let mut slices: BTreeMap<(String, String), Vec<&EndpointSlice>> = BTreeMap::new();
        for slice in &inputs.endpointslices {
            let ns = slice
                .metadata
                .namespace
                .clone()
                .unwrap_or_else(|| "default".to_string());
            let svc = slice
                .metadata
                .labels
                .as_ref()
                .and_then(|l| l.get(SERVICE_NAME_LABEL).cloned());
            if let Some(svc) = svc {
                slices.entry((ns, svc)).or_default().push(slice);
            }
        }
        Self {
            services,
            secrets,
            slices,
        }
    }
}

/// Resolve an Ingress backend (service + port) to (cluster_id, pod addresses).
fn resolve_backends(
    index: &Index,
    namespace: &str,
    service: &str,
    port_ref: &PortRef,
) -> Result<(String, i32, Vec<SocketAddr>), Problem> {
    let svc = index
        .services
        .get(&(namespace.to_string(), service.to_string()))
        .ok_or_else(|| Problem::ServiceNotFound {
            service: service.to_string(),
        })?;

    let ports = svc.spec.as_ref().and_then(|s| s.ports.as_ref());
    let port = ports.and_then(|ports| {
        ports.iter().find(|p| match port_ref {
            PortRef::Number(n) => p.port == *n,
            PortRef::Name(name) => p.name.as_deref() == Some(name.as_str()),
        })
    });
    let svc_port = port.ok_or_else(|| Problem::ServicePortNotFound {
        service: service.to_string(),
        port: match port_ref {
            PortRef::Number(n) => n.to_string(),
            PortRef::Name(n) => n.clone(),
        },
    })?;

    // EndpointSlice port whose name matches the Service port name (both may be None).
    let want_port_name = svc_port.name.clone();
    let cluster_id = format!("{namespace}.{service}.{}", svc_port.port);

    let mut addrs = Vec::new();
    if let Some(slices) = index
        .slices
        .get(&(namespace.to_string(), service.to_string()))
    {
        for slice in slices {
            let pod_port = slice
                .ports
                .as_ref()
                .and_then(|ports| {
                    // Match the EndpointSlice port by name (the Service port name).
                    // Only fall back to the sole port when there is exactly one —
                    // never guess `first()` on a multi-port slice (would route to
                    // the wrong container port).
                    ports.iter().find(|p| p.name == want_port_name).or_else(|| {
                        if ports.len() == 1 {
                            ports.first()
                        } else {
                            None
                        }
                    })
                })
                .and_then(|p| p.port);
            let Some(pod_port) = pod_port else { continue };

            for endpoint in slice.endpoints.iter().flatten() {
                // Treat ready=None (unknown) as ready; exclude only ready=Some(false).
                let ready = endpoint
                    .conditions
                    .as_ref()
                    .and_then(|c| c.ready)
                    .unwrap_or(true);
                if !ready {
                    continue;
                }
                for addr in &endpoint.addresses {
                    if let Ok(ip) = addr.parse::<IpAddr>() {
                        addrs.push(SocketAddr::new(ip, pod_port as u16));
                    }
                }
            }
        }
    }

    addrs.sort();
    addrs.dedup();
    Ok((cluster_id, svc_port.port, addrs))
}

/// Resolve a Service+port and upsert its cluster + pod-IP backends into the
/// shared accumulators. Shared by the Ingress and Gateway API mappers so both
/// feed the *same* IR. Returns `(cluster_id, has_ready_endpoints)`.
pub(crate) fn add_service_route(
    index: &Index,
    clusters: &mut BTreeMap<String, ir::Cluster>,
    backends: &mut BTreeMap<String, ir::Backend>,
    namespace: &str,
    service: &str,
    port_ref: &PortRef,
) -> Result<(String, bool), Problem> {
    let (cluster_id, _port, addrs) = resolve_backends(index, namespace, service, port_ref)?;
    clusters.entry(cluster_id.clone()).or_insert(ir::Cluster {
        id: cluster_id.clone(),
        load_balancing: ir::LbAlgorithm::RoundRobin,
        sticky_session: false,
        https_redirect: false,
    });
    for addr in &addrs {
        let backend_id = format!("{cluster_id}#{addr}");
        backends.entry(backend_id.clone()).or_insert(ir::Backend {
            cluster_id: cluster_id.clone(),
            backend_id,
            address: *addr,
            weight: None,
        });
    }
    Ok((cluster_id, !addrs.is_empty()))
}

/// Compile all our-class Ingresses (+ resolved deps) into the IR.
pub fn build(cfg: &BuildConfig, inputs: &Inputs) -> BuildOutput {
    let index = Index::build(inputs);

    let mut clusters: BTreeMap<String, ir::Cluster> = BTreeMap::new();
    let mut backends: BTreeMap<String, ir::Backend> = BTreeMap::new();
    let mut frontends: Vec<ir::Frontend> = Vec::new();
    let mut certificates: Vec<ir::Certificate> = Vec::new();
    let mut results: Vec<IngressResult> = Vec::new();

    for ingress in &inputs.ingresses {
        if !is_ours(ingress, cfg) {
            continue;
        }
        let (namespace, name) = meta_nn(&ingress.metadata.namespace, &ingress.metadata.name);
        let mut problems: Vec<Problem> = Vec::new();
        let Some(spec) = &ingress.spec else {
            results.push(IngressResult {
                namespace,
                name,
                accepted: true,
                problems,
            });
            continue;
        };

        // ---- TLS: load certs; only hosts with a *successfully loaded* cert
        // become HTTPS-enabled (a frontend without a cert can't handshake) ----
        let mut tls_ready_hosts: BTreeSet<String> = BTreeSet::new();
        for tls in spec.tls.iter().flatten() {
            let hosts: Vec<String> = tls.hosts.clone().unwrap_or_default();
            let Some(secret_name) = &tls.secret_name else {
                problems.push(Problem::TlsEntryWithoutSecret);
                continue;
            };
            match index.secrets.get(&(namespace.clone(), secret_name.clone())) {
                None => problems.push(Problem::SecretNotFound {
                    secret: secret_name.clone(),
                }),
                Some(secret) => match extract_cert(secret) {
                    Ok((leaf, chain, key)) => {
                        for h in &hosts {
                            tls_ready_hosts.insert(h.clone());
                        }
                        certificates.push(ir::Certificate {
                            listener: cfg.https_listener,
                            certificate: leaf,
                            chain,
                            key,
                            names: hosts,
                        });
                    }
                    Err(reason) => problems.push(Problem::InvalidCertificate {
                        secret: secret_name.clone(),
                        reason,
                    }),
                },
            }
        }

        // ---- Rules: clusters + backends + frontends ----
        for rule in spec.rules.iter().flatten() {
            let Some(http) = &rule.http else { continue };
            let Some(host) = &rule.host else {
                problems.push(Problem::HostlessRuleSkipped);
                continue;
            };
            for path in &http.paths {
                let Some(svc_backend) = &path.backend.service else {
                    problems.push(Problem::NonServiceBackend);
                    continue;
                };
                let port_ref = match &svc_backend.port {
                    Some(p) if p.number.is_some() => PortRef::Number(p.number.unwrap_or_default()),
                    Some(p) if p.name.is_some() => {
                        PortRef::Name(p.name.clone().unwrap_or_default())
                    }
                    _ => {
                        problems.push(Problem::ServicePortNotFound {
                            service: svc_backend.name.clone(),
                            port: "<unspecified>".to_string(),
                        });
                        continue;
                    }
                };

                match add_service_route(
                    &index,
                    &mut clusters,
                    &mut backends,
                    &namespace,
                    &svc_backend.name,
                    &port_ref,
                ) {
                    Err(problem) => problems.push(problem),
                    Ok((cluster_id, has_endpoints)) => {
                        if !has_endpoints {
                            problems.push(Problem::NoReadyEndpoints {
                                service: svc_backend.name.clone(),
                            });
                        }

                        let pm = path_match(&path.path_type, path.path.as_deref());
                        frontends.push(ir::Frontend {
                            hostname: host.clone(),
                            path: pm.clone(),
                            method: None,
                            cluster_id: Some(cluster_id.clone()),
                            tls: false,
                            listener: cfg.http_listener,
                            filters: ir::FrontendFilters::default(),
                        });
                        if tls_covers(&tls_ready_hosts, host) {
                            frontends.push(ir::Frontend {
                                hostname: host.clone(),
                                path: pm,
                                method: None,
                                cluster_id: Some(cluster_id),
                                tls: true,
                                listener: cfg.https_listener,
                                filters: ir::FrontendFilters::default(),
                            });
                        }
                    }
                }
            }
        }

        results.push(IngressResult {
            namespace,
            name,
            accepted: true,
            problems,
        });
    }

    // Gateway API (Phase 2): same accumulators, same IR.
    let gw = gateway::build_gateway(
        cfg,
        inputs,
        &index,
        &mut clusters,
        &mut backends,
        &mut frontends,
        &mut certificates,
    );

    // Deterministic frontend ordering (the Translator re-canonicalises anyway).
    frontends.sort_by(|a, b| {
        (a.tls, &a.hostname, &a.cluster_id).cmp(&(b.tls, &b.hostname, &b.cluster_id))
    });

    // Dedup certificates referenced by multiple Ingresses / TLS entries.
    certificates.sort_by(|a, b| {
        (&a.listener, &a.names, &a.certificate).cmp(&(&b.listener, &b.names, &b.certificate))
    });
    certificates.dedup_by(|a, b| {
        a.listener == b.listener && a.names == b.names && a.certificate == b.certificate
    });

    BuildOutput {
        ir: ir::Ir {
            clusters: clusters.into_values().collect(),
            backends: backends.into_values().collect(),
            frontends,
            certificates,
        },
        results,
        gateway_classes: gw.classes,
        gateways: gw.gateways,
        routes: gw.routes,
    }
}
