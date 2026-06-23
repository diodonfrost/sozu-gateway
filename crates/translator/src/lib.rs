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
//! backends → certificates → frontends; removes in reverse). Frontend *removes*
//! are ordered before frontend *adds*: Sōzu keys a route by host+path (not by
//! cluster_id), so re-pointing a route at another cluster is a Remove+Add on the
//! same key, and adding first would be rejected as a duplicate. A new/replacement
//! certificate lands before the old one is removed → no TLS gap. This also makes
//! the otherwise HashSet-ordered routing diff deterministic.
#![forbid(unsafe_code)]

use std::collections::{BTreeSet, HashMap};
use std::net::SocketAddr;

use sozu_command_lib::proto::command::{
    request::RequestType, AddBackend, AddCertificate, CertificateAndKey, Cluster, Header,
    HeaderPosition, LoadBalancingAlgorithms, LoadBalancingParams, PathRule, PathRuleKind,
    RedirectPolicy, RedirectScheme, RemoveCertificate, ReplaceCertificate, Request,
    RequestHttpFrontend, RulePosition,
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
    let mut payload = RequestHttpFrontend {
        cluster_id: Some(f.cluster_id.clone()),
        address: f.listener.into(),
        hostname: f.hostname.clone(),
        path: path_rule(&f.path),
        method: f.method.clone(),
        position: RulePosition::Tree as i32,
        ..Default::default()
    };
    apply_filters(&mut payload, &f.filters);
    if f.tls {
        RequestType::AddHttpsFrontend(payload).into()
    } else {
        RequestType::AddHttpFrontend(payload).into()
    }
}

/// Map the IR's per-route filters onto Sōzu's frontend fields.
fn apply_filters(payload: &mut RequestHttpFrontend, filters: &ir::FrontendFilters) {
    payload.headers = filters
        .header_mods
        .iter()
        .map(|m| Header {
            position: match m.on {
                ir::HeaderTarget::Request => HeaderPosition::Request,
                ir::HeaderTarget::Response => HeaderPosition::Response,
            } as i32,
            key: m.key.clone(),
            // Empty value deletes the header by name (Sōzu semantics).
            val: m.value.clone().unwrap_or_default(),
        })
        .collect();

    if let Some(redirect) = &filters.redirect {
        payload.redirect = Some(match redirect.status {
            ir::RedirectStatus::MovedPermanently => RedirectPolicy::Permanent,
            ir::RedirectStatus::Found => RedirectPolicy::Found,
        } as i32);
        if let Some(scheme) = redirect.scheme {
            payload.redirect_scheme = Some(match scheme {
                ir::Scheme::Http => RedirectScheme::UseHttp,
                ir::Scheme::Https => RedirectScheme::UseHttps,
            } as i32);
        }
    }

    if let Some(rewrite) = &filters.rewrite {
        payload.rewrite_host = rewrite.hostname.clone();
        payload.rewrite_path = rewrite.path.clone();
    }
}

/// Frontends deduplicated by their Sōzu route key (tls + listener + hostname +
/// path + method). Sōzu rejects a duplicate AddHttpFrontend, so a benign
/// duplicate produced by overlapping Ingresses must not become a hard reconcile
/// failure. First occurrence wins.
fn unique_frontends(ir: &ir::Ir) -> Vec<&ir::Frontend> {
    let mut seen: BTreeSet<(bool, SocketAddr, &str, &ir::PathMatch, Option<&str>)> =
        BTreeSet::new();
    ir.frontends
        .iter()
        .filter(|f| {
            seen.insert((
                f.tls,
                f.listener,
                f.hostname.as_str(),
                &f.path,
                f.method.as_deref(),
            ))
        })
        .collect()
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

/// SNI name set (order-insensitive) for rotation pairing.
fn names_set(c: &ir::Certificate) -> BTreeSet<&str> {
    c.names.iter().map(String::as_str).collect()
}

fn remove_certificate_request(listener: SocketAddr, fingerprint: String) -> Request {
    RequestType::RemoveCertificate(RemoveCertificate {
        address: listener.into(),
        fingerprint,
    })
    .into()
}

fn replace_certificate_request(new: &ir::Certificate, old_fingerprint: String) -> Request {
    RequestType::ReplaceCertificate(ReplaceCertificate {
        address: new.listener.into(),
        new_certificate: certificate_and_key(new),
        old_fingerprint,
        new_expired_at: None,
    })
    .into()
}

/// A certificate keyed by (listener, fingerprint) — Sōzu's own identity for a
/// loaded cert (`HashMap<SocketAddr, HashMap<Fingerprint, _>>`).
struct KeyedCert<'a> {
    listener: SocketAddr,
    fingerprint: String,
    cert: &'a ir::Certificate,
}

fn keyed_certs(certs: &[ir::Certificate]) -> Result<Vec<KeyedCert<'_>>, TranslatorError> {
    certs
        .iter()
        .map(|c| {
            Ok(KeyedCert {
                listener: c.listener,
                fingerprint: fingerprint(c)?,
                cert: c,
            })
        })
        .collect()
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
        // Frontend removes precede frontend adds. Sōzu keys a route by
        // `address;hostname;path[;method]` (cluster_id is NOT part of the key),
        // so re-pointing a host+path at a different cluster yields a Remove(old)
        // + Add(new) on the *same* key. Add-before-Remove would make the live
        // `add_http_frontend` hit an Occupied entry → `StateError::Exists`, and
        // the trailing Remove would then delete the route outright. Removing
        // first leaves the entry Vacant for the re-add (a tiny, unavoidable gap
        // since Sōzu 2.1.0 has no atomic frontend replace).
        Some(RequestType::RemoveHttpFrontend(_)) | Some(RequestType::RemoveHttpsFrontend(_)) => 4,
        Some(RequestType::AddHttpFrontend(_)) | Some(RequestType::AddHttpsFrontend(_)) => 5,
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
    requests.extend(unique_frontends(ir).into_iter().map(frontend_request));
    let mut state = ConfigState::new();
    for req in canonicalize(requests) {
        state
            .dispatch(&req)
            .map_err(|e| TranslatorError::Dispatch(e.to_string()))?;
    }
    Ok(state)
}

/// Minimal certificate requests to converge `previous` → `desired`. Identity is
/// (listener, fingerprint) — matching Sōzu's own cert store — so the same cert on
/// two listeners is tracked independently. Handles:
///  - new cert at (listener, fp)        -> AddCertificate
///  - cert gone from (listener, fp)     -> RemoveCertificate
///  - same (listener, fp), names differ -> ReplaceCertificate (same fp; Sōzu
///    skips a plain AddCertificate whose fp already exists, so a Replace is the
///    only way to update SNI names in place)
///  - rotation (a removed + an added at the same listener sharing the SNI name
///    set) -> a single ReplaceCertificate (zero-gap)
fn certificate_requests(
    previous: &[ir::Certificate],
    desired: &[ir::Certificate],
) -> Result<Vec<Request>, TranslatorError> {
    let prev = keyed_certs(previous)?;
    let des = keyed_certs(desired)?;

    let prev_by_key: HashMap<(SocketAddr, &str), &KeyedCert> = prev
        .iter()
        .map(|k| ((k.listener, k.fingerprint.as_str()), k))
        .collect();
    let des_keys: BTreeSet<(SocketAddr, &str)> = des
        .iter()
        .map(|k| (k.listener, k.fingerprint.as_str()))
        .collect();

    let mut out = Vec::new();
    let mut truly_added: Vec<&KeyedCert> = Vec::new();

    for d in &des {
        match prev_by_key.get(&(d.listener, d.fingerprint.as_str())) {
            // Same (listener, fp): in place. Only a name change needs a request.
            Some(p) => {
                if names_set(p.cert) != names_set(d.cert) {
                    out.push(replace_certificate_request(d.cert, d.fingerprint.clone()));
                }
            }
            None => truly_added.push(d),
        }
    }

    let mut truly_removed: Vec<&KeyedCert> = prev
        .iter()
        .filter(|p| !des_keys.contains(&(p.listener, p.fingerprint.as_str())))
        .collect();

    // Pair an add with a removal at the same listener + same SNI names -> rotate.
    let mut used = vec![false; truly_removed.len()];
    for new in &truly_added {
        if let Some(idx) = truly_removed.iter().enumerate().position(|(i, old)| {
            !used[i] && old.listener == new.listener && names_set(old.cert) == names_set(new.cert)
        }) {
            used[idx] = true;
            out.push(replace_certificate_request(
                new.cert,
                truly_removed[idx].fingerprint.clone(),
            ));
        } else {
            out.push(add_certificate_request(new.cert));
        }
    }
    for (i, old) in truly_removed.iter_mut().enumerate() {
        if !used[i] {
            out.push(remove_certificate_request(
                old.listener,
                old.fingerprint.clone(),
            ));
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
    requests.extend(unique_frontends(ir).into_iter().map(frontend_request));
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
