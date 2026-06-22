//! Translator: pure IR → Sōzu protobuf commands.
//!
//! Two responsibilities, both side-effect free and golden-file tested:
//!  1. map the IR to the full set of `Add*` requests that express the desired
//!     state (and fold them into a `ConfigState`);
//!  2. diff the desired `ConfigState` against the last-applied (shadow) state,
//!     reusing Sōzu's own `ConfigState::diff` so the semantics match the data
//!     plane exactly, and emit only the minimal mutations.
//!
//! Request ordering is canonicalised into **dependency-safe tiers** (adds:
//! clusters → backends → certificates → frontends; removes in reverse; a new
//! certificate is added before the old one is removed, so rotation has no gap).
//! The same ordering is used for sending and for snapshots, which also makes the
//! otherwise HashSet-ordered `diff` output deterministic.
#![forbid(unsafe_code)]

use sozu_command_lib::proto::command::{
    request::RequestType, AddBackend, AddCertificate, CertificateAndKey, Cluster,
    LoadBalancingAlgorithms, LoadBalancingParams, PathRule, PathRuleKind, Request,
    RequestHttpFrontend, RulePosition,
};
use sozu_command_lib::state::ConfigState;
use sozu_gw_ir as ir;
use thiserror::Error;

#[derive(Debug, Error)]
pub enum TranslatorError {
    #[error("failed to fold request into ConfigState: {0}")]
    Dispatch(String),
}

// ----------------------------------------------------------------------------
// IR element -> single request
// ----------------------------------------------------------------------------

fn lb_algorithm(algo: ir::LbAlgorithm) -> i32 {
    let v = match algo {
        ir::LbAlgorithm::RoundRobin => LoadBalancingAlgorithms::RoundRobin,
        ir::LbAlgorithm::Random => LoadBalancingAlgorithms::Random,
        ir::LbAlgorithm::LeastLoaded => LoadBalancingAlgorithms::LeastLoaded,
        ir::LbAlgorithm::PowerOfTwo => LoadBalancingAlgorithms::PowerOfTwo,
    };
    v as i32
}

fn path_rule(path: &ir::PathMatch) -> PathRule {
    let (kind, value) = match path {
        ir::PathMatch::Prefix(v) => (PathRuleKind::Prefix, v.clone()),
        ir::PathMatch::Exact(v) => (PathRuleKind::Equals, v.clone()),
        ir::PathMatch::Regex(v) => (PathRuleKind::Regex, v.clone()),
    };
    PathRule {
        kind: kind as i32,
        value,
    }
}

fn cluster_request(c: &ir::Cluster) -> Request {
    RequestType::AddCluster(Cluster {
        cluster_id: c.id.clone(),
        sticky_session: c.sticky_session,
        https_redirect: c.https_redirect,
        load_balancing: lb_algorithm(c.load_balancing),
        ..Default::default()
    })
    .into()
}

fn backend_request(b: &ir::Backend) -> Request {
    RequestType::AddBackend(AddBackend {
        cluster_id: b.cluster_id.clone(),
        backend_id: b.backend_id.clone(),
        // Always use the crate's conversion — never hand-pack the address.
        address: b.address.into(),
        sticky_id: None,
        load_balancing_parameters: b.weight.map(|weight| LoadBalancingParams { weight }),
        backup: None,
    })
    .into()
}

fn frontend_request(f: &ir::Frontend) -> Request {
    let payload = RequestHttpFrontend {
        cluster_id: Some(f.cluster_id.clone()),
        address: f.listener.into(),
        hostname: f.hostname.clone(),
        path: path_rule(&f.path),
        method: f.method.clone(),
        position: RulePosition::Tree as i32,
        ..Default::default()
    };
    if f.tls {
        RequestType::AddHttpsFrontend(payload).into()
    } else {
        RequestType::AddHttpFrontend(payload).into()
    }
}

fn certificate_request(c: &ir::Certificate) -> Request {
    RequestType::AddCertificate(AddCertificate {
        address: c.listener.into(),
        certificate: CertificateAndKey {
            certificate: c.certificate.clone(),
            certificate_chain: c.chain.clone(),
            key: c.key.clone(),
            versions: vec![], // empty => server default (TLS 1.2 + 1.3)
            names: c.names.clone(),
        },
        expired_at: None,
    })
    .into()
}

// ----------------------------------------------------------------------------
// Dependency-safe canonical ordering
// ----------------------------------------------------------------------------

/// Tier of a request in dependency-safe apply order. Lower tiers are applied
/// first. Adds create dependencies bottom-up (cluster before backend before
/// frontend); removes tear down top-down; a replacement cert is added (tier 3)
/// before the stale one is removed (tier 8), so TLS never has a gap.
fn tier(req: &Request) -> u8 {
    match &req.request_type {
        Some(RequestType::AddHttpListener(_))
        | Some(RequestType::AddHttpsListener(_))
        | Some(RequestType::AddTcpListener(_))
        | Some(RequestType::AddUdpListener(_))
        | Some(RequestType::ActivateListener(_)) => 0,
        Some(RequestType::AddCluster(_)) => 1,
        Some(RequestType::AddBackend(_)) => 2,
        Some(RequestType::AddCertificate(_)) | Some(RequestType::ReplaceCertificate(_)) => 3,
        Some(RequestType::AddHttpFrontend(_)) | Some(RequestType::AddHttpsFrontend(_)) => 4,
        Some(RequestType::RemoveHttpFrontend(_)) | Some(RequestType::RemoveHttpsFrontend(_)) => 5,
        Some(RequestType::RemoveBackend(_)) => 6,
        Some(RequestType::RemoveCluster(_)) => 7,
        Some(RequestType::RemoveCertificate(_)) => 8,
        Some(RequestType::DeactivateListener(_)) | Some(RequestType::RemoveListener(_)) => 9,
        _ => 100,
    }
}

/// Reorder requests into dependency-safe tiers with a deterministic secondary
/// key (stable JSON), so the output is both correct to apply and snapshot-stable.
fn canonicalize(mut requests: Vec<Request>) -> Vec<Request> {
    requests.sort_by_cached_key(|req| {
        let key = serde_json::to_string(req).unwrap_or_default();
        (tier(req), key)
    });
    requests
}

// ----------------------------------------------------------------------------
// Public API
// ----------------------------------------------------------------------------

/// The full desired state expressed as `Add*` requests, in canonical order.
pub fn ir_to_requests(ir: &ir::Ir) -> Vec<Request> {
    let mut requests = Vec::with_capacity(
        ir.clusters.len() + ir.backends.len() + ir.frontends.len() + ir.certificates.len(),
    );
    requests.extend(ir.clusters.iter().map(cluster_request));
    requests.extend(ir.backends.iter().map(backend_request));
    requests.extend(ir.frontends.iter().map(frontend_request));
    requests.extend(ir.certificates.iter().map(certificate_request));
    canonicalize(requests)
}

/// Fold the IR into a `ConfigState` (the desired data-plane state). Dispatching
/// in canonical order guarantees clusters exist before their backends.
pub fn desired_state(ir: &ir::Ir) -> Result<ConfigState, TranslatorError> {
    let mut state = ConfigState::new();
    for req in ir_to_requests(ir) {
        state
            .dispatch(&req)
            .map_err(|e| TranslatorError::Dispatch(e.to_string()))?;
    }
    Ok(state)
}

/// Minimal, dependency-safe requests to converge `current` → `desired`,
/// reusing Sōzu's own diff. Idempotent: `diff(&s, &s)` is empty.
pub fn diff(current: &ConfigState, desired: &ConfigState) -> Vec<Request> {
    canonicalize(current.diff(desired))
}

/// Convenience: compute both the new shadow state and the minimal requests to
/// converge a `current` shadow towards the desired IR. The caller swaps its
/// shadow to the returned state only after the requests apply successfully.
pub fn reconcile(
    current: &ConfigState,
    ir: &ir::Ir,
) -> Result<(ConfigState, Vec<Request>), TranslatorError> {
    let desired = desired_state(ir)?;
    let requests = diff(current, &desired);
    Ok((desired, requests))
}
