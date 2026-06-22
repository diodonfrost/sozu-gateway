//! Translator: pure IR → Sōzu protobuf commands.
//!
//! Side-effect free and golden-file tested. Two diff strategies, deliberately
//! split:
//!  - **Routing graph** (clusters / backends / frontends): we reuse Sōzu's own
//!    `ConfigState::diff`, so the semantics match the data plane exactly.
//!  - **Certificates**: we diff them ourselves, by fingerprint. This (a) lets us
//!    emit `ReplaceCertificate` for zero-gap rotation, and (b) avoids a
//!    debug-assert in sozu-command-lib 2.1.0 that fires when `ConfigState::diff`
//!    removes the last certificate at a listener address (an empty cert bucket
//!    is left behind and the replay check is not normalised for it).
//!
//! Output is canonicalised into dependency-safe tiers (adds: clusters →
//! backends → certificates → frontends; removes in reverse; a new/replacement
//! certificate lands before the old one is removed → no TLS gap). This also
//! makes the otherwise HashSet-ordered routing diff deterministic.
#![forbid(unsafe_code)]

use std::collections::{BTreeSet, HashSet};
use std::net::SocketAddr;

use sozu_command_lib::proto::command::{
    request::RequestType, AddBackend, AddCertificate, CertificateAndKey, Cluster,
    LoadBalancingAlgorithms, LoadBalancingParams, PathRule, PathRuleKind, RemoveCertificate,
    ReplaceCertificate, Request, RequestHttpFrontend, RulePosition,
};
use sozu_command_lib::state::ConfigState;
use sozu_gw_ir as ir;
use thiserror::Error;

#[derive(Debug, Error)]
pub enum TranslatorError {
    #[error("failed to fold request into ConfigState: {0}")]
    Dispatch(String),
    #[error("invalid certificate (cannot compute fingerprint): {0}")]
    Certificate(String),
}

// ----------------------------------------------------------------------------
// IR element -> request payloads
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

fn certificate_and_key(c: &ir::Certificate) -> CertificateAndKey {
    CertificateAndKey {
        certificate: c.certificate.clone(),
        certificate_chain: c.chain.clone(),
        key: c.key.clone(),
        versions: vec![], // empty => server default (TLS 1.2 + 1.3)
        names: c.names.clone(),
    }
}

fn add_certificate_request(c: &ir::Certificate) -> Request {
    RequestType::AddCertificate(AddCertificate {
        address: c.listener.into(),
        certificate: certificate_and_key(c),
        expired_at: None,
    })
    .into()
}

/// Lower-case hex fingerprint of a certificate's leaf, matching the form Sōzu
/// stores and `RemoveCertificate` expects.
fn fingerprint(c: &ir::Certificate) -> Result<String, TranslatorError> {
    let bytes = sozu_command_lib::certificate::calculate_fingerprint(c.certificate.as_bytes())
        .map_err(|e| TranslatorError::Certificate(format!("{e:?}")))?;
    Ok(bytes.iter().map(|b| format!("{b:02x}")).collect())
}

/// Identity for rotation detection: same listener + same SNI name set.
fn cert_key(c: &ir::Certificate) -> (SocketAddr, BTreeSet<String>) {
    (c.listener, c.names.iter().cloned().collect())
}

// ----------------------------------------------------------------------------
// Dependency-safe canonical ordering
// ----------------------------------------------------------------------------

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

/// Reorder into dependency-safe tiers with a deterministic secondary key.
fn canonicalize(mut requests: Vec<Request>) -> Vec<Request> {
    requests.sort_by_cached_key(|req| {
        let key = serde_json::to_string(req).unwrap_or_default();
        (tier(req), key)
    });
    requests
}

// ----------------------------------------------------------------------------
// Diff building blocks
// ----------------------------------------------------------------------------

/// Fold the IR's routing graph (clusters/backends/frontends, NO certificates)
/// into a `ConfigState`. Certificates are handled separately, so they never
/// enter the `ConfigState::diff` path.
fn routing_state(ir: &ir::Ir) -> Result<ConfigState, TranslatorError> {
    let mut requests: Vec<Request> = Vec::new();
    requests.extend(ir.clusters.iter().map(cluster_request));
    requests.extend(ir.backends.iter().map(backend_request));
    requests.extend(ir.frontends.iter().map(frontend_request));
    let mut state = ConfigState::new();
    for req in canonicalize(requests) {
        state
            .dispatch(&req)
            .map_err(|e| TranslatorError::Dispatch(e.to_string()))?;
    }
    Ok(state)
}

/// Minimal certificate requests to converge `previous` → `desired`, pairing a
/// removed+added cert that share (listener, names) into a single
/// `ReplaceCertificate` (zero-gap rotation).
fn certificate_requests(
    previous: &[ir::Certificate],
    desired: &[ir::Certificate],
) -> Result<Vec<Request>, TranslatorError> {
    let prev: Vec<(String, &ir::Certificate)> = previous
        .iter()
        .map(|c| Ok((fingerprint(c)?, c)))
        .collect::<Result<_, TranslatorError>>()?;
    let des: Vec<(String, &ir::Certificate)> = desired
        .iter()
        .map(|c| Ok((fingerprint(c)?, c)))
        .collect::<Result<_, TranslatorError>>()?;

    let prev_fps: HashSet<&str> = prev.iter().map(|(f, _)| f.as_str()).collect();
    let des_fps: HashSet<&str> = des.iter().map(|(f, _)| f.as_str()).collect();

    let removed: Vec<(String, &ir::Certificate)> = prev
        .iter()
        .filter(|(f, _)| !des_fps.contains(f.as_str()))
        .map(|(f, c)| (f.clone(), *c))
        .collect();
    let added: Vec<(String, &ir::Certificate)> = des
        .iter()
        .filter(|(f, _)| !prev_fps.contains(f.as_str()))
        .map(|(f, c)| (f.clone(), *c))
        .collect();

    let mut out = Vec::new();
    let mut used = vec![false; removed.len()];

    for (_new_fp, new_cert) in &added {
        // Rotation: a removed cert with the same (listener, names) -> Replace.
        if let Some(idx) = removed
            .iter()
            .enumerate()
            .position(|(i, (_, old))| !used[i] && cert_key(old) == cert_key(new_cert))
        {
            used[idx] = true;
            out.push(
                RequestType::ReplaceCertificate(ReplaceCertificate {
                    address: new_cert.listener.into(),
                    new_certificate: certificate_and_key(new_cert),
                    old_fingerprint: removed[idx].0.clone(),
                    new_expired_at: None,
                })
                .into(),
            );
        } else {
            out.push(add_certificate_request(new_cert));
        }
    }
    for (i, (old_fp, old_cert)) in removed.iter().enumerate() {
        if !used[i] {
            out.push(
                RequestType::RemoveCertificate(RemoveCertificate {
                    address: old_cert.listener.into(),
                    fingerprint: old_fp.clone(),
                })
                .into(),
            );
        }
    }
    Ok(out)
}

// ----------------------------------------------------------------------------
// Public API
// ----------------------------------------------------------------------------

/// The full desired state expressed as `Add*` requests, in canonical order.
/// Pure mapping (no diff/replay) — handy for a fresh "apply everything" and for
/// golden snapshots of the IR → command mapping.
pub fn ir_to_requests(ir: &ir::Ir) -> Vec<Request> {
    let mut requests = Vec::new();
    requests.extend(ir.clusters.iter().map(cluster_request));
    requests.extend(ir.backends.iter().map(backend_request));
    requests.extend(ir.frontends.iter().map(frontend_request));
    requests.extend(ir.certificates.iter().map(add_certificate_request));
    canonicalize(requests)
}

/// Minimal, dependency-safe requests to converge a `previous` applied IR towards
/// the `desired` IR. Idempotent: `reconcile(&ir, &ir)` is empty. The controller
/// keeps `previous` as its shadow and swaps it to `desired` only after a
/// successful apply.
pub fn reconcile(previous: &ir::Ir, desired: &ir::Ir) -> Result<Vec<Request>, TranslatorError> {
    let mut requests = routing_state(previous)?.diff(&routing_state(desired)?);
    requests.extend(certificate_requests(
        &previous.certificates,
        &desired.certificates,
    )?);
    Ok(canonicalize(requests))
}
