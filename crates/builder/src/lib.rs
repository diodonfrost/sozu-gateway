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
use std::sync::Arc;

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
    /// Externally advertised port for HTTP Gateway listeners: the port clients
    /// connect to (the LoadBalancer Service's exposed port), which a Gateway's
    /// `listener.port` declares. Distinct from `http_listener`, the pod-level
    /// bind — under the chart defaults the Service maps 80 → 8080.
    pub gateway_http_port: u16,
    /// Externally advertised port for HTTPS Gateway listeners (see
    /// [`Self::gateway_http_port`]).
    pub gateway_https_port: u16,
}

impl Default for BuildConfig {
    fn default() -> Self {
        Self {
            class_name: "sozu".to_string(),
            class_is_default: false,
            controller_name: "sozu.io/gateway-controller".to_string(),
            http_listener: "0.0.0.0:80".parse().expect("const addr"),
            https_listener: "0.0.0.0:443".parse().expect("const addr"),
            gateway_http_port: 80,
            gateway_https_port: 443,
        }
    }
}

/// Already-fetched cluster objects. The controller pushes everything it watches;
/// `build` indexes and resolves. Order-independent.
///
/// The collections hold `Arc`s so the controller can hand its reflector-cache
/// entries straight through (kube-rs stores yield `Arc<K>`): the build only
/// reads them, and deep-cloning every cached object on every reconcile would
/// cost tens of MB per pass on a busy cluster. `Arc` is plain shared memory —
/// the crate stays I/O-free.
#[derive(Default)]
pub struct Inputs {
    pub ingresses: Vec<Arc<Ingress>>,
    pub services: Vec<Arc<Service>>,
    pub endpointslices: Vec<Arc<EndpointSlice>>,
    pub secrets: Vec<Arc<Secret>>,
    // Gateway API (Phase 2). Empty when only Ingress is in use.
    pub gateway_classes: Vec<Arc<GatewayClass>>,
    pub gateways: Vec<Arc<Gateway>>,
    pub http_routes: Vec<Arc<HttpRoute>>,
    pub reference_grants: Vec<Arc<ReferenceGrant>>,
    // L4 (TCP/UDP) port→service maps, ingress-nginx style (`"<port>": "ns/svc:port"`).
    pub tcp_services: Option<ConfigMap>,
    pub udp_services: Option<ConfigMap>,
}

/// A problem found while building one Ingress. Surfaced for status + logging;
/// non-fatal problems still let the rest of the Ingress translate.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub enum Problem {
    SecretNotFound {
        secret: String,
    },
    TlsEntryWithoutSecret,
    InvalidCertificate {
        secret: String,
        reason: String,
    },
    NonServiceBackend,
    /// `spec.defaultBackend` is not mapped onto Sōzu routing: mapping it onto
    /// Sōzu's catch-all semantics is unverified, so per the honesty rule it
    /// is reported, never approximated. `spec.rules` still translate.
    DefaultBackendUnsupported,
    ServiceNotFound {
        service: String,
    },
    ServicePortNotFound {
        service: String,
        port: String,
    },
    NoReadyEndpoints {
        service: String,
    },
    /// Another object already claims this exact route key (listener + host +
    /// path + method) with a different effect. This route is dropped — never
    /// silently applied on top — in favour of `winner` (a cluster id, or
    /// `"<redirect>"` for a cluster-less redirect frontend). Applies to
    /// Ingress and Gateway API routes alike.
    RouteCollision {
        hostname: String,
        path: String,
        winner: String,
    },
    // Gateway API (Phase 2) — features Sōzu or this phase does not cover yet.
    UnsupportedTlsMode {
        mode: String,
    },
    UnsupportedProtocol {
        protocol: String,
    },
    /// An HTTP/HTTPS listener declares a port that differs from the
    /// externally advertised port for its protocol
    /// ([`BuildConfig::gateway_http_port`]/[`BuildConfig::gateway_https_port`]).
    /// `listener.port` is the client-visible port — the LoadBalancer
    /// Service's exposed port, not the pod bind — and the gateway only
    /// serves the advertised ones (Sōzu's HTTP(S) listeners are fixed at
    /// boot); the listener is reported and NOT programmed — landing its
    /// routes on the advertised port would silently serve traffic on a port
    /// the Gateway never declared.
    ListenerPortMismatch {
        listener: String,
        declared: i32,
        expected: u16,
    },
    WeightedBackendsUnsupported,
    /// A single `backendRef` with `weight: 0` (the standard drain pattern)
    /// must receive no traffic; with every weight zero the spec even calls
    /// for a 500 on matching requests. Sōzu can neither weight backends nor
    /// synthesize that 500, so the rule is reported and skipped (fail
    /// closed) instead of serving the drained backend 100% of the traffic.
    ZeroWeightBackendUnsupported {
        service: String,
    },
    /// `rule.timeouts` has no Sōzu equivalent; the rule still routes,
    /// without the timeout, and the gap is reported.
    TimeoutsUnsupported,
    HeaderOrQueryMatchUnsupported,
    /// A listener's `allowedRoutes.namespaces.from: Selector` cannot be
    /// evaluated (there is no Namespace label index), so the listener fails
    /// CLOSED — it admits no routes at all — rather than silently admitting
    /// every namespace on a control the Gateway owner meant to restrict.
    NamespaceSelectorUnsupported {
        listener: String,
    },
    FilterUnsupported {
        kind: String,
    },
    BackendRefNotPermitted {
        reference: String,
    },
    // L4 (TCP/UDP) services.
    InvalidL4Mapping {
        entry: String,
    },
    L4PortReserved {
        port: u16,
    },
}

impl Problem {
    /// Machine-readable CamelCase reason (the variant name) — the shape
    /// Kubernetes wants for Event reasons and condition reasons.
    pub fn reason(&self) -> &'static str {
        match self {
            Problem::SecretNotFound { .. } => "SecretNotFound",
            Problem::TlsEntryWithoutSecret => "TlsEntryWithoutSecret",
            Problem::InvalidCertificate { .. } => "InvalidCertificate",
            Problem::NonServiceBackend => "NonServiceBackend",
            Problem::DefaultBackendUnsupported => "DefaultBackendUnsupported",
            Problem::ServiceNotFound { .. } => "ServiceNotFound",
            Problem::ServicePortNotFound { .. } => "ServicePortNotFound",
            Problem::NoReadyEndpoints { .. } => "NoReadyEndpoints",
            Problem::RouteCollision { .. } => "RouteCollision",
            Problem::UnsupportedTlsMode { .. } => "UnsupportedTlsMode",
            Problem::UnsupportedProtocol { .. } => "UnsupportedProtocol",
            Problem::ListenerPortMismatch { .. } => "ListenerPortMismatch",
            Problem::WeightedBackendsUnsupported => "WeightedBackendsUnsupported",
            Problem::ZeroWeightBackendUnsupported { .. } => "ZeroWeightBackendUnsupported",
            Problem::TimeoutsUnsupported => "TimeoutsUnsupported",
            Problem::HeaderOrQueryMatchUnsupported => "HeaderOrQueryMatchUnsupported",
            Problem::NamespaceSelectorUnsupported { .. } => "NamespaceSelectorUnsupported",
            Problem::FilterUnsupported { .. } => "FilterUnsupported",
            Problem::BackendRefNotPermitted { .. } => "BackendRefNotPermitted",
            Problem::InvalidL4Mapping { .. } => "InvalidL4Mapping",
            Problem::L4PortReserved { .. } => "L4PortReserved",
        }
    }

    /// The listener this problem is scoped to, when the variant carries one —
    /// lets per-listener status conditions cite their own problems.
    pub fn listener(&self) -> Option<&str> {
        match self {
            Problem::ListenerPortMismatch { listener, .. }
            | Problem::NamespaceSelectorUnsupported { listener } => Some(listener),
            _ => None,
        }
    }
}

/// Human one-liner carrying the *detail* (which Secret, which Service, which
/// port) — what status condition messages and Events show to users, so they
/// can self-diagnose without controller log access.
impl std::fmt::Display for Problem {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Problem::SecretNotFound { secret } => write!(f, "TLS Secret {secret:?} not found"),
            Problem::TlsEntryWithoutSecret => {
                write!(f, "Ingress TLS entry has no secretName; its hosts stay plain HTTP")
            }
            Problem::InvalidCertificate { secret, reason } => {
                write!(f, "TLS Secret {secret:?} holds unusable material: {reason}")
            }
            Problem::NonServiceBackend => write!(f, "only Service backends are supported"),
            Problem::DefaultBackendUnsupported => {
                write!(f, "spec.defaultBackend is not supported and was ignored (rules still apply)")
            }
            Problem::ServiceNotFound { service } => {
                write!(f, "backend Service {service:?} not found")
            }
            Problem::ServicePortNotFound { service, port } => {
                write!(f, "port {port:?} not found on Service {service:?}")
            }
            Problem::NoReadyEndpoints { service } => {
                write!(f, "Service {service:?} has no ready endpoints")
            }
            Problem::RouteCollision {
                hostname,
                path,
                winner,
            } => write!(
                f,
                "host+path {hostname}{path} is already served by {winner}; this route was dropped"
            ),
            Problem::UnsupportedTlsMode { mode } => {
                write!(f, "TLS mode {mode:?} is not supported (Terminate only)")
            }
            Problem::UnsupportedProtocol { protocol } => {
                write!(f, "listener protocol {protocol:?} is not supported (HTTP/HTTPS only)")
            }
            Problem::ListenerPortMismatch {
                listener,
                declared,
                expected,
            } => write!(
                f,
                "listener {listener:?} declares port {declared} but this gateway only serves the advertised port {expected}"
            ),
            Problem::WeightedBackendsUnsupported => {
                write!(f, "weighted multi-backend splits are not supported (Sōzu cannot weight backends)")
            }
            Problem::ZeroWeightBackendUnsupported { service } => write!(
                f,
                "backendRef {service:?} with weight 0 cannot be honoured (Sōzu cannot drain by weight); the rule was skipped"
            ),
            Problem::TimeoutsUnsupported => {
                write!(f, "rule.timeouts has no Sōzu equivalent; the rule routes without it")
            }
            Problem::HeaderOrQueryMatchUnsupported => {
                write!(f, "header/query matches are not supported by Sōzu; the rule was skipped")
            }
            Problem::NamespaceSelectorUnsupported { listener } => write!(
                f,
                "listener {listener:?} uses allowedRoutes.namespaces.from: Selector, which cannot be evaluated; the listener admits no routes (fail closed)"
            ),
            Problem::FilterUnsupported { kind } => write!(f, "unsupported filter: {kind}"),
            Problem::BackendRefNotPermitted { reference } => write!(
                f,
                "cross-namespace reference {reference} is not permitted (no ReferenceGrant covers it)"
            ),
            Problem::InvalidL4Mapping { entry } => {
                write!(f, "invalid L4 service mapping: {entry}")
            }
            Problem::L4PortReserved { port } => {
                write!(f, "L4 port {port} is reserved by the gateway's own listeners")
            }
        }
    }
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

/// PEM labels we accept for `tls.key` (PKCS#8, PKCS#1 and SEC1).
const KEY_PEM_LABELS: [&str; 3] = ["PRIVATE KEY", "RSA PRIVATE KEY", "EC PRIVATE KEY"];

/// Extract **and validate** leaf + chain + key PEM from a TLS Secret
/// (`tls.crt` / `tls.key`). Returns `(leaf, chain, key, leaf_fingerprint)`;
/// the fingerprint is the SHA-256 of the leaf's DER — Sōzu's identity for the
/// certificate.
///
/// `split_certificate_chain` is purely textual (it scans for the BEGIN/END
/// markers) and never decodes the base64 body, so everything is parsed here,
/// *before* it enters the IR: a corrupt certificate would otherwise abort the
/// translator's whole diff (freezing convergence for every namespace), and a
/// corrupt key would make Sōzu reject the `AddCertificate` at apply time —
/// certificates tier before frontends, so that blocks every frontend add.
pub(crate) fn extract_cert(
    secret: &Secret,
) -> Result<(String, Vec<String>, String, Vec<u8>), String> {
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

    // Fully parse the leaf — PEM framing, base64 body AND the decoded DER —
    // with the same helpers Sōzu applies to an `AddCertificate` (`parse_pem` +
    // `parse_x509`, its X509 parse for CN/SAN extraction). Hashing the decoded
    // bytes alone would wave through valid-base64 garbage that Sōzu then
    // rejects at apply time, aborting the whole batch on every cycle. The
    // fingerprint is the SHA-256 of the DER, the translator's identity.
    let leaf_pem = sozu_command_lib::certificate::parse_pem(leaf.as_bytes())
        .map_err(|e| format!("invalid certificate in tls.crt: {e}"))?;
    sozu_command_lib::certificate::parse_x509(&leaf_pem.contents)
        .map_err(|e| format!("invalid certificate in tls.crt: {e}"))?;
    let fingerprint =
        sozu_command_lib::certificate::calculate_fingerprint_from_der(&leaf_pem.contents);
    // The intermediates ride in the same AddCertificate, so a corrupt one
    // would equally be rejected by Sōzu at apply time.
    for (i, c) in chain.iter().enumerate() {
        let pem = sozu_command_lib::certificate::parse_pem(c.as_bytes())
            .map_err(|e| format!("invalid chain certificate #{} in tls.crt: {e}", i + 1))?;
        sozu_command_lib::certificate::parse_x509(&pem.contents)
            .map_err(|e| format!("invalid chain certificate #{} in tls.crt: {e}", i + 1))?;
    }
    // Sanity-check the key: it must at least be a well-formed PEM block with a
    // plausible private-key label (full key/cert pairing is Sōzu's job).
    let key_pem = sozu_command_lib::certificate::parse_pem(key.as_bytes())
        .map_err(|e| format!("invalid private key in tls.key: {e}"))?;
    if !KEY_PEM_LABELS.contains(&key_pem.label.as_str()) {
        return Err(format!(
            "tls.key is not a private key (PEM label {:?})",
            key_pem.label
        ));
    }

    Ok((leaf, chain, key, fingerprint))
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
            services.insert(
                meta_nn(&svc.metadata.namespace, &svc.metadata.name),
                svc.as_ref(),
            );
        }
        let mut secrets = BTreeMap::new();
        for secret in &inputs.secrets {
            secrets.insert(
                meta_nn(&secret.metadata.namespace, &secret.metadata.name),
                secret.as_ref(),
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
                slices.entry((ns, svc)).or_default().push(slice.as_ref());
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

/// Where an HTTP(S) frontend came from, so a route-key collision can be
/// reported on the losing object's own result.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) enum FrontendSource {
    Ingress {
        namespace: String,
        name: String,
    },
    /// An HTTPRoute rule, attributed to one (route, parentRef) pair —
    /// collision problems land on that parent's [`RouteParentResult`]. The
    /// parentRef's `sectionName`/`port` are part of the identity: a route may
    /// carry several parentRefs to the *same* Gateway, and the collision must
    /// land on the one that produced the losing frontend.
    HttpRoute {
        namespace: String,
        name: String,
        gateway_namespace: String,
        gateway_name: String,
        section_name: Option<String>,
        port: Option<i32>,
    },
}

/// An IR frontend paired with its source, carried until collision resolution.
pub(crate) struct SourcedFrontend {
    pub(crate) frontend: ir::Frontend,
    pub(crate) source: FrontendSource,
}

/// Mirror of Sōzu's route key: the listener a frontend binds to (`tls` picks
/// the HTTPS vs HTTP listener), hostname, path match, optional method. The
/// target cluster is *not* part of the key.
type RouteKey = (bool, SocketAddr, String, ir::PathMatch, Option<String>);

/// The raw path value of a match, for problem context.
fn path_value(p: &ir::PathMatch) -> &str {
    match p {
        ir::PathMatch::Prefix(v) | ir::PathMatch::Exact(v) | ir::PathMatch::Regex(v) => v,
    }
}

/// Keep exactly one frontend per Sōzu route key and report the losers.
///
/// Sōzu keys a route by `address;hostname;path[;method]` — the cluster is NOT
/// part of the key — so two frontends sharing a key cannot coexist. The
/// translator already dedups on that key, first occurrence wins, over the
/// builder's `(tls, hostname, cluster_id)` ordering; the winner kept here
/// replicates exactly that (smallest by the sort, a cluster-less redirect —
/// `None` — ordering before any cluster id), so reporting the collision does
/// not change observable routing. A future improvement could prefer
/// oldest-object-wins instead. Byte-identical duplicates (same target
/// cluster, same filters) are benign overlaps — Sōzu would apply either one
/// with the same effect — and stay unreported; a loser with a *different*
/// effect gets a [`Problem::RouteCollision`] attributed to its source.
fn resolve_frontend_collisions(
    mut frontends: Vec<SourcedFrontend>,
) -> (Vec<ir::Frontend>, Vec<(FrontendSource, Problem)>) {
    // Stable sort: ties keep emission order, like the previous IR ordering.
    frontends.sort_by(|a, b| {
        (a.frontend.tls, &a.frontend.hostname, &a.frontend.cluster_id).cmp(&(
            b.frontend.tls,
            &b.frontend.hostname,
            &b.frontend.cluster_id,
        ))
    });

    let mut kept: Vec<ir::Frontend> = Vec::new();
    let mut winners: BTreeMap<RouteKey, usize> = BTreeMap::new();
    let mut collisions: Vec<(FrontendSource, Problem)> = Vec::new();
    for sf in frontends {
        let key: RouteKey = (
            sf.frontend.tls,
            sf.frontend.listener,
            sf.frontend.hostname.clone(),
            sf.frontend.path.clone(),
            sf.frontend.method.clone(),
        );
        match winners.get(&key) {
            None => {
                winners.insert(key, kept.len());
                kept.push(sf.frontend);
            }
            Some(&i) => {
                let winner = &kept[i];
                if winner.cluster_id == sf.frontend.cluster_id
                    && winner.filters == sf.frontend.filters
                {
                    continue; // benign duplicate: same route, same effect
                }
                collisions.push((
                    sf.source,
                    Problem::RouteCollision {
                        hostname: sf.frontend.hostname.clone(),
                        path: path_value(&sf.frontend.path).to_string(),
                        winner: winner
                            .cluster_id
                            .clone()
                            .unwrap_or_else(|| "<redirect>".to_string()),
                    },
                ));
            }
        }
    }
    (kept, collisions)
}

/// An IR certificate paired with its leaf fingerprint (SHA-256 of the DER, as
/// computed by [`extract_cert`]) — Sōzu's identity for a loaded certificate.
/// Carried until [`merge_certificates`] so the merge keys on the DER identity,
/// not on PEM bytes.
pub(crate) struct FingerprintedCert {
    pub(crate) fingerprint: Vec<u8>,
    pub(crate) cert: ir::Certificate,
}

/// Merge certificates that share Sōzu's identity at a listener — same leaf
/// `(listener, fingerprint)` — into one entry, unioning their SNI names. The
/// same certificate routinely backs several routes with different hostnames
/// (e.g. an Ingress and a Gateway listener); Sōzu stores exactly one
/// certificate per `(listener, fingerprint)`, so the IR must present one too.
/// Otherwise the translator would emit a `ReplaceCertificate` on every
/// reconcile, forever flipping between the conflicting name sets. Keying on
/// the fingerprint (not the PEM text) matters: two Secrets holding the same
/// DER re-encoded with different line wrapping (cert-manager vs hand-made)
/// are still one certificate to Sōzu. The first occurrence fixes the entry's
/// position and PEM bytes (the DER is identical anyway).
fn merge_certificates(certs: Vec<FingerprintedCert>) -> Vec<ir::Certificate> {
    let mut merged: Vec<FingerprintedCert> = Vec::new();
    for c in certs {
        match merged
            .iter_mut()
            .find(|e| e.cert.listener == c.cert.listener && e.fingerprint == c.fingerprint)
        {
            Some(existing) => existing.cert.names.extend(c.cert.names),
            None => merged.push(c),
        }
    }
    let mut merged: Vec<ir::Certificate> = merged.into_iter().map(|c| c.cert).collect();
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
    let mut frontends: Vec<SourcedFrontend> = Vec::new();
    let mut certificates: Vec<FingerprintedCert> = Vec::new();
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
                problems,
            });
            continue;
        };

        let source = FrontendSource::Ingress {
            namespace: namespace.clone(),
            name: name.clone(),
        };

        // spec.defaultBackend has no verified Sōzu equivalent; an Ingress
        // relying on it would otherwise build to nothing while reading
        // accepted-with-no-problems (its requests just 404). Report it and
        // translate the rules as usual.
        if spec.default_backend.is_some() {
            problems.push(Problem::DefaultBackendUnsupported);
        }

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
                    Ok((leaf, chain, key, fingerprint)) => {
                        for h in &hosts {
                            tls_ready_hosts.insert(h.clone());
                        }
                        certificates.push(FingerprintedCert {
                            fingerprint,
                            cert: ir::Certificate {
                                listener: cfg.https_listener,
                                certificate: leaf,
                                chain,
                                key,
                                names: hosts,
                            },
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
                        frontends.push(SourcedFrontend {
                            frontend: ir::Frontend {
                                hostname: host.clone(),
                                path: pm.clone(),
                                method: None,
                                cluster_id: Some(cluster_id.clone()),
                                tls: false,
                                listener: cfg.http_listener,
                                filters: http_filters,
                            },
                            source: source.clone(),
                        });
                        if host_has_tls {
                            frontends.push(SourcedFrontend {
                                frontend: ir::Frontend {
                                    hostname: host.clone(),
                                    path: pm,
                                    method: None,
                                    cluster_id: Some(cluster_id),
                                    tls: true,
                                    listener: cfg.https_listener,
                                    filters: ir::FrontendFilters::default(),
                                },
                                source: source.clone(),
                            });
                        }
                    }
                }
            }
        }

        results.push(IngressResult {
            namespace,
            name,
            problems,
        });
    }

    // Gateway API (Phase 2): same accumulators, same IR.
    let mut gw = gateway::build_gateway(
        cfg,
        inputs,
        &index,
        &mut clusters,
        &mut backends,
        &mut frontends,
        &mut certificates,
    );

    // Deterministic frontend ordering (the Translator re-canonicalises anyway)
    // + one frontend per Sōzu route key: a route-key collision with a
    // different effect is reported on the losing owner's result instead of
    // silently letting the winner steal the traffic.
    let (frontends, collisions) = resolve_frontend_collisions(frontends);
    for (source, problem) in collisions {
        // Dedup: the HTTP and HTTPS frontends of one host+path lose as a pair
        // to the same winner — one problem carries the same information.
        match source {
            // No Ingress status machinery exists, so a losing Ingress keeps
            // problem-only reporting.
            FrontendSource::Ingress { namespace, name } => {
                if let Some(r) = results
                    .iter_mut()
                    .find(|r| r.namespace == namespace && r.name == name)
                {
                    if !r.problems.contains(&problem) {
                        r.problems.push(problem);
                    }
                }
            }
            FrontendSource::HttpRoute {
                namespace,
                name,
                gateway_namespace,
                gateway_name,
                section_name,
                port,
            } => {
                if let Some(p) = gw
                    .routes
                    .iter_mut()
                    .find(|r| r.namespace == namespace && r.name == name)
                    .and_then(|r| {
                        // The parentRef identity includes sectionName/port: a
                        // route may hold several parentRefs to one Gateway,
                        // and the loser must be the parent that produced the
                        // colliding frontend, not the first match.
                        r.parents.iter_mut().find(|p| {
                            p.gateway_namespace == gateway_namespace
                                && p.gateway_name == gateway_name
                                && p.section_name == section_name
                                && p.port == port
                        })
                    })
                {
                    // A losing route must not read fully healthy: downgrade
                    // Accepted with an implementation-specific reason so the
                    // condition (not just the log) carries the collision.
                    p.accepted = false;
                    p.accepted_reason = "RouteCollision";
                    if !p.problems.contains(&problem) {
                        p.problems.push(problem);
                    }
                }
            }
        }
    }

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
