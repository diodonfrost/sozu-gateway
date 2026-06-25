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

use k8s_openapi::api::core::v1::{ConfigMap, Secret, Service};
use k8s_openapi::api::discovery::v1::EndpointSlice;
use k8s_openapi::api::networking::v1::Ingress;
use serde::Serialize;
use sozu_gw_gateway_api::{Gateway, GatewayClass, HttpRoute, ReferenceGrant};
use sozu_gw_ir as ir;

mod gateway;
pub use gateway::{
    GatewayClassResult, GatewayResult, ListenerStatus, RouteParentResult, RouteResult,
};

const SERVICE_NAME_LABEL: &str = "kubernetes.io/service-name";
const LEGACY_CLASS_ANNOTATION: &str = "kubernetes.io/ingress.class";
/// Service annotation selecting the cluster's load-balancing algorithm:
/// `round-robin` (default) | `random` | `least-loaded` | `power-of-two`.
const LB_ANNOTATION: &str = "sozu.io/load-balancing";
/// Service annotation enabling sticky sessions when set to `"true"`.
const STICKY_ANNOTATION: &str = "sozu.io/sticky-sessions";
/// Service annotation capping simultaneous connections per source IP (u64).
const MAX_CONN_PER_IP_ANNOTATION: &str = "sozu.io/max-connections-per-ip";
/// Service annotation setting the `Retry-After` seconds on the cap's `429` (u32).
const RETRY_AFTER_ANNOTATION: &str = "sozu.io/retry-after";
/// Ingress annotation to opt out of the automatic HTTP→HTTPS redirect. The
/// redirect is on by default for any host that has a loaded TLS cert; set this
/// to `"false"` to keep serving plain HTTP.
const SSL_REDIRECT_ANNOTATION: &str = "sozu.io/ssl-redirect";

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
    // L4 (TCP/UDP) port→service maps, ingress-nginx style (`"<port>": "ns/svc:port"`).
    pub tcp_services: Option<ConfigMap>,
    pub udp_services: Option<ConfigMap>,
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
    // Gateway API (Phase 2) — features Sōzu or this phase does not cover yet.
    UnsupportedTlsMode { mode: String },
    UnsupportedProtocol { protocol: String },
    WeightedBackendsUnsupported,
    HeaderOrQueryMatchUnsupported,
    FilterUnsupported { kind: String },
    BackendRefNotPermitted { reference: String },
    // L4 (TCP/UDP) services.
    InvalidL4Mapping { entry: String },
    L4PortReserved { port: u16 },
}

/// Result of one L4 (TCP/UDP) port mapping, for status/logging.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct L4Result {
    pub protocol: String,
    pub listen_port: u16,
    pub target: String,
    pub problems: Vec<Problem>,
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
    /// L4 (TCP/UDP) port-mapping results.
    pub l4_results: Vec<L4Result>,
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

/// Parse the `sozu.io/load-balancing` annotation value. Unknown values (and the
/// absence of the annotation) fall back to round-robin — a missing/typo'd value
/// keeps a valid, predictable default rather than failing the whole Service.
fn parse_lb_algorithm(value: &str) -> ir::LbAlgorithm {
    match value
        .trim()
        .to_ascii_lowercase()
        .replace([' ', '_'], "-")
        .as_str()
    {
        "random" => ir::LbAlgorithm::Random,
        "least-loaded" => ir::LbAlgorithm::LeastLoaded,
        "power-of-two" => ir::LbAlgorithm::PowerOfTwo,
        _ => ir::LbAlgorithm::RoundRobin,
    }
}

/// Cluster-level settings read from the backing Service's annotations. The
/// cluster is 1:1 with a Service, so these live on the Service (not the route) —
/// both Ingress and Gateway refs to one Service then agree on one cluster
/// config, with no cross-route conflict.
#[derive(Default)]
struct ClusterSettings {
    load_balancing: ir::LbAlgorithm,
    sticky_session: bool,
    max_connections_per_ip: Option<u64>,
    retry_after: Option<u32>,
}

fn cluster_settings(service: Option<&Service>) -> ClusterSettings {
    let annotations = service.and_then(|s| s.metadata.annotations.as_ref());
    let get = |key: &str| annotations.and_then(|a| a.get(key)).map(|v| v.trim());
    ClusterSettings {
        load_balancing: get(LB_ANNOTATION)
            .map(parse_lb_algorithm)
            .unwrap_or_default(),
        sticky_session: get(STICKY_ANNOTATION).is_some_and(|v| v.eq_ignore_ascii_case("true")),
        // A non-numeric value is ignored (falls back to the global default)
        // rather than failing the Service.
        max_connections_per_ip: get(MAX_CONN_PER_IP_ANNOTATION).and_then(|v| v.parse().ok()),
        retry_after: get(RETRY_AFTER_ANNOTATION).and_then(|v| v.parse().ok()),
    }
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
    let svc = index
        .services
        .get(&(namespace.to_string(), service.to_string()))
        .copied();
    let s = cluster_settings(svc);
    clusters.entry(cluster_id.clone()).or_insert(ir::Cluster {
        id: cluster_id.clone(),
        load_balancing: s.load_balancing,
        sticky_session: s.sticky_session,
        https_redirect: false,
        max_connections_per_ip: s.max_connections_per_ip,
        retry_after: s.retry_after,
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

/// Merge certificates that share Sōzu's identity at a listener — same leaf PEM
/// (hence same fingerprint) on the same listener — into one entry, unioning
/// their SNI names. The same TLS Secret routinely backs several routes with
/// different hostnames (e.g. an Ingress and a Gateway listener); Sōzu stores
/// exactly one certificate per `(listener, fingerprint)`, so the IR must present
/// one too. Otherwise the translator would emit a `ReplaceCertificate` on every
/// reconcile, forever flipping between the conflicting name sets.
fn merge_certificates(certs: Vec<ir::Certificate>) -> Vec<ir::Certificate> {
    let mut merged: Vec<ir::Certificate> = Vec::new();
    for c in certs {
        match merged
            .iter_mut()
            .find(|e| e.listener == c.listener && e.certificate == c.certificate)
        {
            Some(existing) => existing.names.extend(c.names),
            None => merged.push(c),
        }
    }
    for c in &mut merged {
        c.names.sort();
        c.names.dedup();
    }
    merged
}

/// Parse one `tcp/udp-services` entry: key `"<listen-port>"`, value
/// `"<namespace>/<service>:<service-port>"` (a `service-port` may be a number or
/// a name; any extra `:`-suffix like ingress-nginx's `:PROXY` is ignored).
fn parse_l4_entry(key: &str, value: &str) -> Option<(u16, String, String, PortRef)> {
    let listen_port: u16 = key.trim().parse().ok()?;
    let (namespace, rest) = value.trim().split_once('/')?;
    let mut parts = rest.split(':');
    let service = parts.next()?;
    let svc_port = parts.next()?;
    if namespace.is_empty() || service.is_empty() || svc_port.is_empty() {
        return None;
    }
    let port_ref = match svc_port.parse::<i32>() {
        Ok(n) => PortRef::Number(n),
        Err(_) => PortRef::Name(svc_port.to_string()),
    };
    Some((
        listen_port,
        namespace.to_string(),
        service.to_string(),
        port_ref,
    ))
}

/// Compile the `tcp/udp-services` ConfigMaps into L4 frontends (+ per-port
/// results), resolving each target Service to pod-IP backends via the same path
/// as HTTP. L4 has no host multiplexing: one listen port → one Service.
fn build_l4(
    cfg: &BuildConfig,
    index: &Index,
    clusters: &mut BTreeMap<String, ir::Cluster>,
    backends: &mut BTreeMap<String, ir::Backend>,
    inputs: &Inputs,
) -> (Vec<ir::L4Frontend>, Vec<L4Result>) {
    let mut l4_frontends = Vec::new();
    let mut results = Vec::new();
    // Ports already bound by the static HTTP/HTTPS listeners can't be reused.
    let reserved = [cfg.http_listener.port(), cfg.https_listener.port()];

    for (protocol, label, cm) in [
        (ir::L4Protocol::Tcp, "TCP", inputs.tcp_services.as_ref()),
        (ir::L4Protocol::Udp, "UDP", inputs.udp_services.as_ref()),
    ] {
        let Some(cm) = cm else { continue };
        for (key, value) in cm.data.iter().flatten() {
            let mut problems = Vec::new();
            match parse_l4_entry(key, value) {
                None => problems.push(Problem::InvalidL4Mapping {
                    entry: format!("{key}: {value}"),
                }),
                Some((port, _, _, _)) if reserved.contains(&port) => {
                    problems.push(Problem::L4PortReserved { port })
                }
                Some((port, ns, svc, port_ref)) => {
                    match add_service_route(index, clusters, backends, &ns, &svc, &port_ref) {
                        Ok((cluster_id, has_endpoints)) => {
                            if !has_endpoints {
                                problems.push(Problem::NoReadyEndpoints { service: svc });
                            }
                            l4_frontends.push(ir::L4Frontend {
                                protocol,
                                listener: SocketAddr::new(cfg.http_listener.ip(), port),
                                cluster_id,
                            });
                        }
                        Err(problem) => problems.push(problem),
                    }
                }
            }
            results.push(L4Result {
                protocol: label.to_string(),
                listen_port: key.trim().parse().unwrap_or(0),
                target: value.clone(),
                problems,
            });
        }
    }
    (l4_frontends, results)
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

        // Automatic HTTP→HTTPS redirect: on by default, opt out per Ingress.
        let ssl_redirect = ingress
            .metadata
            .annotations
            .as_ref()
            .and_then(|a| a.get(SSL_REDIRECT_ANNOTATION))
            .map(|v| !v.trim().eq_ignore_ascii_case("false"))
            .unwrap_or(true);

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
            // A hostless rule becomes a catch-all (`*`) frontend, which Sōzu
            // routes as DomainRule::Any. `tls_covers` returns false for `*`, so
            // it stays plain-HTTP (no `*`-named cert frontend).
            let host = rule.host.clone().unwrap_or_else(|| "*".to_string());
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
                        let host_has_tls = tls_covers(&tls_ready_hosts, &host);
                        // The plain-HTTP frontend redirects to HTTPS when the host
                        // has a cert and the redirect isn't opted out; otherwise it
                        // proxies. The redirect wins over the cluster, so keeping
                        // `cluster_id` set is harmless.
                        let http_filters = if host_has_tls && ssl_redirect {
                            ir::FrontendFilters {
                                redirect: Some(ir::Redirect {
                                    scheme: Some(ir::Scheme::Https),
                                    status: ir::RedirectStatus::MovedPermanently,
                                }),
                                ..Default::default()
                            }
                        } else {
                            ir::FrontendFilters::default()
                        };
                        frontends.push(ir::Frontend {
                            hostname: host.clone(),
                            path: pm.clone(),
                            method: None,
                            cluster_id: Some(cluster_id.clone()),
                            tls: false,
                            listener: cfg.http_listener,
                            filters: http_filters,
                        });
                        if host_has_tls {
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

    // Merge certs that share Sōzu's (listener, fingerprint) identity, unioning
    // their names, then order deterministically. This subsumes exact-duplicate
    // dedup and prevents a perpetual ReplaceCertificate diff.
    let mut certificates = merge_certificates(certificates);
    certificates.sort_by(|a, b| {
        (&a.listener, &a.names, &a.certificate).cmp(&(&b.listener, &b.names, &b.certificate))
    });

    // L4 (TCP/UDP): same Service→pod-IP resolver, into the same clusters/backends.
    let (mut l4_frontends, l4_results) =
        build_l4(cfg, &index, &mut clusters, &mut backends, inputs);
    l4_frontends.sort_by_key(|f| (f.protocol, f.listener));

    BuildOutput {
        ir: ir::Ir {
            clusters: clusters.into_values().collect(),
            backends: backends.into_values().collect(),
            frontends,
            certificates,
            l4_frontends,
        },
        results,
        gateway_classes: gw.classes,
        gateways: gw.gateways,
        routes: gw.routes,
        l4_results,
    }
}
