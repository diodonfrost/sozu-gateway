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

use std::collections::{BTreeMap, BTreeSet, HashMap};
use std::net::SocketAddr;

use sozu_command_lib::proto::command::{
    request::RequestType, ActivateListener, AddBackend, AddCertificate, CertificateAndKey, Cluster,
    Header, HeaderPosition, ListenerType, LoadBalancingAlgorithms, LoadBalancingParams, PathRule,
    PathRuleKind, RedirectPolicy, RedirectScheme, RemoveCertificate, ReplaceCertificate, Request,
    RequestHttpFrontend, RequestTcpFrontend, RequestUdpFrontend, RulePosition, TcpListenerConfig,
    UdpListenerConfig,
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
    #[error("conflicting L4 frontends: {0}")]
    L4Conflict(String),
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
        max_connections_per_ip: c.max_connections_per_ip,
        retry_after: c.retry_after,
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
    // A bare "*" catch-all goes in POST so it is a fallback and never shadows
    // specific-host (TREE) frontends. Sōzu parses "*" as DomainRule::Any.
    let position = if f.hostname == "*" {
        RulePosition::Post
    } else {
        RulePosition::Tree
    } as i32;
    let mut payload = RequestHttpFrontend {
        cluster_id: f.cluster_id.clone(),
        address: f.listener.into(),
        hostname: f.hostname.clone(),
        path: path_rule(&f.path),
        method: f.method.clone(),
        position,
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
struct KeyedCert {
    listener: SocketAddr,
    fingerprint: String,
    cert: ir::Certificate,
}

/// Group the certs by (listener, fingerprint), unioning the SNI names within
/// each group. The fingerprint is computed over the parsed DER, so two
/// byte-different PEM encodings of the *same* certificate share one identity in
/// Sōzu; kept as separate entries they would make the diff compare the single
/// loaded cert against whichever duplicate it pairs with — re-emitting a
/// ReplaceCertificate on every cycle and clamping SNI coverage to that entry's
/// names. Grouping first keeps `reconcile(&ir, &ir)` empty and every hostname
/// covered. The first occurrence fixes the group's position and PEM bytes
/// (the DER is identical anyway); the merged name set is sorted.
fn keyed_certs(certs: &[ir::Certificate]) -> Result<Vec<KeyedCert>, TranslatorError> {
    let mut out: Vec<KeyedCert> = Vec::new();
    let mut index: HashMap<(SocketAddr, String), usize> = HashMap::new();
    for c in certs {
        let fp = fingerprint(c)?;
        match index.get(&(c.listener, fp.clone())) {
            Some(&i) => {
                let existing = &mut out[i].cert;
                let mut names: BTreeSet<String> = existing.names.drain(..).collect();
                names.extend(c.names.iter().cloned());
                existing.names = names.into_iter().collect();
            }
            None => {
                index.insert((c.listener, fp.clone()), out.len());
                out.push(KeyedCert {
                    listener: c.listener,
                    fingerprint: fp,
                    cert: c.clone(),
                });
            }
        }
    }
    Ok(out)
}

// ----------------------------------------------------------------------------
// Layer 4 (TCP/UDP)
// ----------------------------------------------------------------------------

fn tcp_listener_add(addr: SocketAddr) -> Request {
    RequestType::AddTcpListener(TcpListenerConfig {
        address: addr.into(),
        public_address: None,
        expect_proxy: false,
        front_timeout: 60,
        back_timeout: 30,
        connect_timeout: 3,
        active: true,
    })
    .into()
}

fn udp_listener_add(addr: SocketAddr) -> Request {
    RequestType::AddUdpListener(UdpListenerConfig {
        address: addr.into(),
        public_address: None,
        front_timeout: 30,
        back_timeout: 30,
        max_rx_datagram_size: 1500,
        max_flows: 0,
        active: true,
    })
    .into()
}

fn activate_listener(addr: SocketAddr, proxy: ListenerType) -> Request {
    RequestType::ActivateListener(ActivateListener {
        address: addr.into(),
        proxy: proxy as i32,
        from_scm: false,
    })
    .into()
}

fn l4_frontend_request(f: &ir::L4Frontend) -> Request {
    match f.protocol {
        ir::L4Protocol::Tcp => RequestType::AddTcpFrontend(RequestTcpFrontend {
            cluster_id: f.cluster_id.clone(),
            address: f.listener.into(),
            tags: Default::default(),
        })
        .into(),
        ir::L4Protocol::Udp => RequestType::AddUdpFrontend(RequestUdpFrontend {
            cluster_id: f.cluster_id.clone(),
            address: f.listener.into(),
            tags: Default::default(),
        })
        .into(),
    }
}

/// L4 frontends deduplicated by exact identity (protocol + listener +
/// cluster). Like [`unique_frontends`], a benign duplicate — two sources
/// mapping the same port to the same cluster — must not hard-fail the whole
/// reconcile with `StateError::Exists`. First occurrence wins. Only *exact*
/// duplicates collapse; conflicting claims are [`check_l4_conflicts`]' job.
fn unique_l4_frontends(l4: &[ir::L4Frontend]) -> Vec<&ir::L4Frontend> {
    let mut seen: BTreeSet<(ir::L4Protocol, SocketAddr, &str)> = BTreeSet::new();
    l4.iter()
        .filter(|f| seen.insert((f.protocol, f.listener, f.cluster_id.as_str())))
        .collect()
}

/// Reject two L4 frontends claiming one listen address for *different*
/// clusters. At L4 there is no host multiplexing — one address routes to
/// exactly one cluster — and `ConfigState` buckets TCP/UDP frontends by
/// cluster, so the fold alone would accept both claims and silently program
/// an ambiguous route. Expects an exact-deduplicated slice: any repeated
/// (protocol, listener) key left is a conflict.
fn check_l4_conflicts(l4: &[&ir::L4Frontend]) -> Result<(), TranslatorError> {
    let mut claims: BTreeMap<(ir::L4Protocol, SocketAddr), &str> = BTreeMap::new();
    for f in l4 {
        if let Some(other) = claims.insert((f.protocol, f.listener), f.cluster_id.as_str()) {
            return Err(TranslatorError::L4Conflict(format!(
                "{} ({:?}) is claimed by both cluster {other:?} and cluster {:?}",
                f.listener, f.protocol, f.cluster_id
            )));
        }
    }
    Ok(())
}

/// `AddTcpListener`/`AddUdpListener` for each distinct L4 listen address (active,
/// so `ConfigState::diff` derives the matching `ActivateListener`).
fn l4_listener_adds(l4: &[ir::L4Frontend]) -> Vec<Request> {
    let mut seen: BTreeSet<(ir::L4Protocol, SocketAddr)> = BTreeSet::new();
    l4.iter()
        .filter(|f| seen.insert((f.protocol, f.listener)))
        .map(|f| match f.protocol {
            ir::L4Protocol::Tcp => tcp_listener_add(f.listener),
            ir::L4Protocol::Udp => udp_listener_add(f.listener),
        })
        .collect()
}

/// Explicit `ActivateListener` for each distinct L4 listen address — needed on
/// the full-apply path (no diff to derive activation from `active = true`).
fn l4_listener_activations(l4: &[ir::L4Frontend]) -> Vec<Request> {
    let mut seen: BTreeSet<(ir::L4Protocol, SocketAddr)> = BTreeSet::new();
    l4.iter()
        .filter(|f| seen.insert((f.protocol, f.listener)))
        .map(|f| {
            let proxy = match f.protocol {
                ir::L4Protocol::Tcp => ListenerType::Tcp,
                ir::L4Protocol::Udp => ListenerType::Udp,
            };
            activate_listener(f.listener, proxy)
        })
        .collect()
}

// ----------------------------------------------------------------------------
// Dependency-safe canonical ordering
// ----------------------------------------------------------------------------

fn tier(req: &Request) -> u8 {
    match &req.request_type {
        // Listeners must exist before they can be activated, and both before any
        // cluster/frontend can attach to them.
        Some(RequestType::AddHttpListener(_))
        | Some(RequestType::AddHttpsListener(_))
        | Some(RequestType::AddTcpListener(_))
        | Some(RequestType::AddUdpListener(_)) => 0,
        Some(RequestType::ActivateListener(_)) => 1,
        Some(RequestType::AddCluster(_)) => 2,
        Some(RequestType::AddBackend(_)) => 3,
        Some(RequestType::AddCertificate(_)) | Some(RequestType::ReplaceCertificate(_)) => 4,
        // Frontend removes precede frontend adds. Sōzu keys a route by
        // `address;hostname;path[;method]` (cluster_id is NOT part of the key),
        // so re-pointing a host+path at a different cluster yields a Remove(old)
        // + Add(new) on the *same* key. Add-before-Remove would make the live
        // `add_http_frontend` hit an Occupied entry → `StateError::Exists`, and
        // the trailing Remove would then delete the route outright. Removing
        // first leaves the entry Vacant for the re-add (a tiny, unavoidable gap
        // since Sōzu 2.1.0 has no atomic frontend replace).
        Some(RequestType::RemoveHttpFrontend(_))
        | Some(RequestType::RemoveHttpsFrontend(_))
        | Some(RequestType::RemoveTcpFrontend(_))
        | Some(RequestType::RemoveUdpFrontend(_)) => 5,
        Some(RequestType::AddHttpFrontend(_))
        | Some(RequestType::AddHttpsFrontend(_))
        | Some(RequestType::AddTcpFrontend(_))
        | Some(RequestType::AddUdpFrontend(_)) => 6,
        Some(RequestType::RemoveBackend(_)) => 7,
        Some(RequestType::RemoveCluster(_)) => 8,
        Some(RequestType::RemoveCertificate(_)) => 9,
        // Listener teardown: deactivate before remove — the order Sōzu itself
        // emits. Explicit consecutive tiers so the order can never silently
        // flip on the lexicographic accident of the serialized request names
        // (the within-tier sort key is the JSON encoding).
        Some(RequestType::DeactivateListener(_)) => 10,
        Some(RequestType::RemoveListener(_)) => 11,
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

/// Drop every `RemoveBackend` whose (cluster_id, backend_id, address) triple
/// also appears as an `AddBackend` in the same batch.
///
/// `ConfigState::diff` emits a *changed* backend (same key, e.g. a new weight)
/// as Remove-then-Add, but `canonicalize` reorders backend adds (tier 3) before
/// backend removes (tier 7), turning that pair into Add-then-Remove. Sōzu's
/// `add_backend` is an upsert and `remove_backend` matches on
/// (backend_id, address) only, so the trailing Remove would delete the backend
/// the Add just updated — leaving the cluster short one live backend. The Add
/// alone already converges, so the Remove is the stale half of the pair and is
/// dropped. A backend whose *address* changed diffs under two different triples
/// and keeps its Remove.
fn drop_superseded_backend_removes(requests: Vec<Request>) -> Vec<Request> {
    let added: BTreeSet<(String, String, SocketAddr)> = requests
        .iter()
        .filter_map(|req| match &req.request_type {
            Some(RequestType::AddBackend(b)) => {
                Some((b.cluster_id.clone(), b.backend_id.clone(), b.address.into()))
            }
            _ => None,
        })
        .collect();
    if added.is_empty() {
        return requests;
    }
    requests
        .into_iter()
        .filter(|req| match &req.request_type {
            Some(RequestType::RemoveBackend(b)) => {
                !added.contains(&(b.cluster_id.clone(), b.backend_id.clone(), b.address.into()))
            }
            _ => true,
        })
        .collect()
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
    // L4: listeners (active=true, so diff derives ActivateListener) + frontends.
    // No explicit ActivateListener here — dispatching it would need the listener
    // to already exist in this transient state, and the diff handles activation.
    let l4 = unique_l4_frontends(&ir.l4_frontends);
    check_l4_conflicts(&l4)?;
    requests.extend(l4_listener_adds(&ir.l4_frontends));
    requests.extend(l4.into_iter().map(l4_frontend_request));
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
/// two listeners is tracked independently. Both sides are grouped by that
/// identity first (`keyed_certs`), so duplicate entries for one cert converge
/// to a single entry carrying the union of their SNI names. Handles:
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
                if names_set(&p.cert) != names_set(&d.cert) {
                    out.push(replace_certificate_request(&d.cert, d.fingerprint.clone()));
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
            !used[i] && old.listener == new.listener && names_set(&old.cert) == names_set(&new.cert)
        }) {
            used[idx] = true;
            out.push(replace_certificate_request(
                &new.cert,
                truly_removed[idx].fingerprint.clone(),
            ));
        } else {
            out.push(add_certificate_request(&new.cert));
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
    // L4: add the listener, activate it, then attach the frontend (tiered).
    requests.extend(l4_listener_adds(&ir.l4_frontends));
    requests.extend(l4_listener_activations(&ir.l4_frontends));
    requests.extend(
        unique_l4_frontends(&ir.l4_frontends)
            .into_iter()
            .map(l4_frontend_request),
    );
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
    let mut requests = canonicalize(drop_superseded_backend_removes(requests));
    // `ConfigState::diff` emits the activation of a newly-added active TCP/UDP
    // listener twice (once inline, once in its trailing activation sweep).
    // After `canonicalize` the batch is fully sorted, so identical requests
    // are adjacent and the duplicate collapses here.
    requests.dedup();
    Ok(requests)
}
