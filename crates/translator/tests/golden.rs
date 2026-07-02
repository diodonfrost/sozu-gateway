//! Golden + property tests for the Translator (IR -> Sōzu commands, and diffs).
//!
//! Snapshots are JSON of the emitted `Vec<Request>`; determinism comes from the
//! canonical tier ordering. Cert tests use real PEM fixtures because Sōzu parses
//! certificates (fingerprint + SNI names).

use std::net::SocketAddr;

use sozu_command_lib::proto::command::request::RequestType;
use sozu_command_lib::proto::command::LoadBalancingParams;
use sozu_command_lib::state::ConfigState;
use sozu_gw_ir as ir;
use sozu_gw_translator as tr;

const CERT_A: &str = include_str!("fixtures/cert_a.pem");
const KEY_A: &str = include_str!("fixtures/key_a.pem");
const CERT_B: &str = include_str!("fixtures/cert_b.pem");
const KEY_B: &str = include_str!("fixtures/key_b.pem");

fn addr(s: &str) -> SocketAddr {
    s.parse().expect("valid socket addr in test")
}

/// Re-wrap a PEM's base64 body at a different line width: byte-different text,
/// identical DER — so Sōzu computes the same fingerprint for both encodings.
fn rewrap_pem(pem: &str) -> String {
    let mut header = "";
    let mut footer = "";
    let mut body = String::new();
    for line in pem.lines() {
        if line.starts_with("-----BEGIN") {
            header = line;
        } else if line.starts_with("-----END") {
            footer = line;
        } else {
            body.push_str(line.trim());
        }
    }
    let wrapped: Vec<&str> = body
        .as_bytes()
        .chunks(48)
        .map(|c| std::str::from_utf8(c).expect("base64 is ASCII"))
        .collect();
    format!("{header}\n{}\n{footer}\n", wrapped.join("\n"))
}

fn cluster(id: &str, lb: ir::LbAlgorithm, sticky: bool) -> ir::Cluster {
    ir::Cluster {
        id: id.to_string(),
        load_balancing: lb,
        sticky_session: sticky,
        https_redirect: false,
        max_connections_per_ip: None,
        retry_after: None,
    }
}

fn backend(cluster_id: &str, addr_s: &str, weight: Option<i32>) -> ir::Backend {
    ir::Backend {
        cluster_id: cluster_id.to_string(),
        backend_id: format!("{cluster_id}-{}", addr_s.replace([':', '.'], "-")),
        address: addr(addr_s),
        weight,
    }
}

fn frontend(host: &str, path: ir::PathMatch, cluster_id: &str, tls: bool) -> ir::Frontend {
    ir::Frontend {
        hostname: host.to_string(),
        path,
        method: None,
        cluster_id: Some(cluster_id.to_string()),
        tls,
        listener: addr(if tls { "0.0.0.0:443" } else { "0.0.0.0:80" }),
        filters: ir::FrontendFilters::default(),
    }
}

fn cert(certificate: &str, key: &str) -> ir::Certificate {
    ir::Certificate {
        listener: addr("0.0.0.0:443"),
        certificate: certificate.to_string(),
        chain: vec![],
        key: key.to_string(),
        names: vec!["app.example.com".to_string()],
    }
}

/// A representative multi-host IR: two clusters, weighted/unweighted backends,
/// HTTP + HTTPS frontends, an exact-path route, and one TLS certificate.
fn sample_ir() -> ir::Ir {
    ir::Ir {
        clusters: vec![
            cluster("app", ir::LbAlgorithm::RoundRobin, false),
            cluster("api", ir::LbAlgorithm::LeastLoaded, true),
        ],
        backends: vec![
            backend("app", "10.0.0.1:8080", None),
            backend("app", "10.0.0.2:8080", None),
            backend("api", "10.0.1.1:9090", Some(5)),
        ],
        frontends: vec![
            frontend(
                "app.example.com",
                ir::PathMatch::Prefix("/".into()),
                "app",
                false,
            ),
            frontend(
                "app.example.com",
                ir::PathMatch::Prefix("/".into()),
                "app",
                true,
            ),
            frontend(
                "api.example.com",
                ir::PathMatch::Exact("/v1".into()),
                "api",
                false,
            ),
        ],
        certificates: vec![cert(CERT_A, KEY_A)],
        l4_frontends: vec![],
    }
}

#[test]
fn ir_to_requests_full() {
    insta::assert_json_snapshot!(tr::ir_to_requests(&sample_ir()));
}

#[test]
fn catch_all_frontend_uses_post_position() {
    // A hostname-less ("*") frontend must use Sōzu's POST rule position (1) so it
    // is a fallback and never shadows specific-host (TREE) frontends.
    let model = ir::Ir {
        clusters: vec![cluster("app", ir::LbAlgorithm::RoundRobin, false)],
        backends: vec![backend("app", "10.0.0.1:8080", None)],
        frontends: vec![frontend(
            "*",
            ir::PathMatch::Prefix("/".into()),
            "app",
            false,
        )],
        certificates: vec![],
        l4_frontends: vec![],
    };
    insta::assert_json_snapshot!(tr::ir_to_requests(&model));
}

#[test]
fn reconcile_from_empty_equals_full_adds() {
    let reqs = tr::reconcile(&ir::Ir::default(), &sample_ir()).expect("reconcile");
    insta::assert_json_snapshot!(reqs);
}

#[test]
fn reconcile_is_idempotent() {
    let sample = sample_ir();
    assert!(
        tr::reconcile(&sample, &sample)
            .expect("reconcile")
            .is_empty(),
        "re-applying an unchanged IR must emit no commands"
    );
}

#[test]
fn reconcile_scale_up_emits_single_add_backend() {
    let mut after = sample_ir();
    after.backends.push(backend("app", "10.0.0.3:8080", None));
    let reqs = tr::reconcile(&sample_ir(), &after).expect("reconcile");
    assert_eq!(reqs.len(), 1, "scale-up should be exactly one request");
    insta::assert_json_snapshot!(reqs);
}

#[test]
fn reconcile_scale_down_emits_single_remove_backend() {
    let mut after = sample_ir();
    after
        .backends
        .retain(|b| b.address != addr("10.0.0.2:8080"));
    let reqs = tr::reconcile(&sample_ir(), &after).expect("reconcile");
    assert_eq!(reqs.len(), 1, "scale-down should be exactly one request");
    insta::assert_json_snapshot!(reqs);
}

#[test]
fn reconcile_weight_change_keeps_backend_alive() {
    // ConfigState::diff emits a changed backend (same cluster/backend/address,
    // new weight) as Remove-then-Add, and canonicalize reorders adds before
    // backend removes. Sōzu's add_backend is an upsert and remove_backend
    // matches on (backend_id, address), so a surviving Remove would delete the
    // backend the Add just updated — it must be dropped, not just reordered.
    let before = sample_ir();
    let mut after = sample_ir();
    for b in &mut after.backends {
        if b.cluster_id == "api" {
            b.weight = Some(7); // was Some(5)
        }
    }
    let reqs = tr::reconcile(&before, &after).expect("reconcile");

    assert!(
        reqs.iter().any(|r| matches!(
            &r.request_type,
            Some(RequestType::AddBackend(b))
                if b.cluster_id == "api"
                    && b.load_balancing_parameters == Some(LoadBalancingParams { weight: 7 })
        )),
        "the weight change must arrive as an AddBackend upsert: {reqs:#?}"
    );
    assert!(
        !reqs
            .iter()
            .any(|r| matches!(r.request_type, Some(RequestType::RemoveBackend(_)))),
        "no RemoveBackend may accompany the upsert (it would delete the backend): {reqs:#?}"
    );
    assert_eq!(reqs.len(), 1, "a weight change is exactly one request");

    // Replay onto Sōzu's own state model, seeded with the before-state: the
    // backend must survive the batch and carry the new weight.
    let mut state = ConfigState::new();
    for req in tr::ir_to_requests(&before).iter().chain(reqs.iter()) {
        state.dispatch(req).expect("dispatch");
    }
    let api = state.backends.get("api").expect("api cluster backends");
    assert_eq!(api.len(), 1, "the api cluster must keep its backend");
    assert_eq!(
        api[0].load_balancing_parameters,
        Some(LoadBalancingParams { weight: 7 }),
        "the surviving backend must carry the new weight"
    );
}

#[test]
fn reconcile_cert_rotation_is_single_replace() {
    let mut after = sample_ir();
    after.certificates = vec![cert(CERT_B, KEY_B)]; // same listener+names, new key/cert
    let reqs = tr::reconcile(&sample_ir(), &after).expect("reconcile");

    assert_eq!(
        reqs.len(),
        1,
        "rotation should be a single ReplaceCertificate"
    );
    assert!(
        matches!(
            reqs[0].request_type,
            Some(RequestType::ReplaceCertificate(_))
        ),
        "rotation with identical (listener, names) must use ReplaceCertificate (no TLS gap)"
    );
    insta::assert_json_snapshot!(reqs);
}

#[test]
fn reconcile_remove_all_includes_remove_certificate() {
    // This is the case that tripped sozu's ConfigState::diff debug-assert; our
    // own certificate diff must handle it without panicking.
    let reqs = tr::reconcile(&sample_ir(), &ir::Ir::default()).expect("reconcile");
    assert!(reqs
        .iter()
        .any(|r| matches!(r.request_type, Some(RequestType::RemoveCertificate(_)))));
    insta::assert_json_snapshot!(reqs);
}

#[test]
fn reconcile_same_cert_on_two_listeners_adds_both() {
    // Identity is (listener, fingerprint): the same PEM on two listeners must
    // produce two AddCertificate, not be deduped to one by fingerprint alone.
    let mut c2 = cert(CERT_A, KEY_A);
    c2.listener = addr("0.0.0.0:8443");
    let desired = ir::Ir {
        certificates: vec![cert(CERT_A, KEY_A), c2],
        ..Default::default()
    };
    let reqs = tr::reconcile(&ir::Ir::default(), &desired).expect("reconcile");
    let adds = reqs
        .iter()
        .filter(|r| matches!(r.request_type, Some(RequestType::AddCertificate(_))))
        .count();
    assert_eq!(adds, 2, "same cert on two listeners must be added on each");
}

#[test]
fn reconcile_cert_name_change_replaces_in_place() {
    // Same PEM (same fingerprint), different SNI names -> ReplaceCertificate
    // (a plain AddCertificate would be skipped by Sōzu as the fp already exists).
    let before = ir::Ir {
        certificates: vec![cert(CERT_A, KEY_A)],
        ..Default::default()
    };
    let mut renamed = cert(CERT_A, KEY_A);
    renamed.names = vec!["app.example.com".into(), "www.example.com".into()];
    let after = ir::Ir {
        certificates: vec![renamed],
        ..Default::default()
    };
    let reqs = tr::reconcile(&before, &after).expect("reconcile");
    assert_eq!(reqs.len(), 1);
    assert!(
        matches!(
            reqs[0].request_type,
            Some(RequestType::ReplaceCertificate(_))
        ),
        "a name-only change must Replace in place, got {:?}",
        reqs[0].request_type
    );
}

#[test]
fn reconcile_duplicate_fingerprint_certs_is_idempotent() {
    // Two byte-different PEM encodings of the SAME certificate (fingerprints
    // are computed over the parsed DER) at one listener, each carrying a
    // different SNI name. They are one identity in Sōzu's cert store, so
    // re-applying the unchanged IR must be a no-op — not a ReplaceCertificate
    // every cycle that clamps SNI coverage to one entry's names.
    let a = cert(CERT_A, KEY_A); // names: app.example.com
    let mut b = cert(&rewrap_pem(CERT_A), KEY_A);
    b.names = vec!["www.example.com".to_string()];

    assert_ne!(a.certificate, b.certificate, "PEMs must be byte-different");
    let fp = |pem: &str| {
        sozu_command_lib::certificate::calculate_fingerprint(pem.as_bytes())
            .expect("valid certificate")
    };
    assert_eq!(
        fp(&a.certificate),
        fp(&b.certificate),
        "both encodings must share one fingerprint"
    );

    let model = ir::Ir {
        certificates: vec![a, b],
        ..Default::default()
    };
    assert!(
        tr::reconcile(&model, &model).expect("reconcile").is_empty(),
        "duplicate-fingerprint entries must reconcile to no requests"
    );
}

#[test]
fn reconcile_duplicate_fingerprint_certs_union_names() {
    // The desired side has duplicate entries for one cert whose SNI name UNION
    // differs from the loaded names: exactly one ReplaceCertificate, carrying
    // the union — neither hostname may lose coverage.
    let before = ir::Ir {
        certificates: vec![cert(CERT_A, KEY_A)], // names: app.example.com
        ..Default::default()
    };
    let mut dup = cert(&rewrap_pem(CERT_A), KEY_A);
    dup.names = vec!["www.example.com".to_string()];
    let after = ir::Ir {
        certificates: vec![cert(CERT_A, KEY_A), dup],
        ..Default::default()
    };
    let reqs = tr::reconcile(&before, &after).expect("reconcile");
    assert_eq!(reqs.len(), 1, "one ReplaceCertificate expected: {reqs:#?}");
    match &reqs[0].request_type {
        Some(RequestType::ReplaceCertificate(r)) => {
            assert_eq!(
                r.new_certificate.names,
                vec!["app.example.com".to_string(), "www.example.com".to_string()],
                "the replacement must carry the union of the SNI names"
            );
        }
        other => panic!("expected ReplaceCertificate, got {other:?}"),
    }
}

#[test]
fn reconcile_retarget_route_removes_before_adds() {
    // Re-pointing the same host+path at a DIFFERENT cluster. Sōzu keys a route by
    // host+path (not cluster_id), so this is a Remove(old)+Add(new) on the same
    // route key. The old frontend MUST be removed before the new one is added —
    // otherwise the live add_http_frontend rejects the duplicate (StateError::Exists)
    // and the trailing remove deletes the route, so the reconcile never converges.
    let before = ir::Ir {
        clusters: vec![cluster("old", ir::LbAlgorithm::RoundRobin, false)],
        backends: vec![backend("old", "10.0.0.1:8080", None)],
        frontends: vec![frontend(
            "app.example.com",
            ir::PathMatch::Prefix("/".into()),
            "old",
            false,
        )],
        certificates: vec![],
        l4_frontends: vec![],
    };
    let after = ir::Ir {
        clusters: vec![cluster("new", ir::LbAlgorithm::RoundRobin, false)],
        backends: vec![backend("new", "10.0.0.2:8080", None)],
        frontends: vec![frontend(
            "app.example.com",
            ir::PathMatch::Prefix("/".into()),
            "new",
            false,
        )],
        certificates: vec![],
        l4_frontends: vec![],
    };
    let reqs = tr::reconcile(&before, &after).expect("reconcile");

    let remove_idx = reqs
        .iter()
        .position(|r| matches!(r.request_type, Some(RequestType::RemoveHttpFrontend(_))))
        .expect("a RemoveHttpFrontend for the old route");
    let add_idx = reqs
        .iter()
        .position(|r| matches!(r.request_type, Some(RequestType::AddHttpFrontend(_))))
        .expect("an AddHttpFrontend for the new route");
    assert!(
        remove_idx < add_idx,
        "old frontend must be removed before the new one is added (same Sōzu route key), \
         got remove at {remove_idx}, add at {add_idx}: {reqs:#?}"
    );
    insta::assert_json_snapshot!(reqs);
}

#[test]
fn reconcile_retarget_https_route_removes_before_adds() {
    // Same retarget but on a TLS frontend: RemoveHttpsFrontend must precede
    // AddHttpsFrontend. Assertion-only (the HTTP case already pins a snapshot).
    let ir_for = |cluster_id: &str, addr_s: &str| ir::Ir {
        clusters: vec![cluster(cluster_id, ir::LbAlgorithm::RoundRobin, false)],
        backends: vec![backend(cluster_id, addr_s, None)],
        frontends: vec![frontend(
            "app.example.com",
            ir::PathMatch::Prefix("/".into()),
            cluster_id,
            true,
        )],
        certificates: vec![cert(CERT_A, KEY_A)],
        l4_frontends: vec![],
    };
    let before = ir_for("old", "10.0.0.1:8080");
    let after = ir_for("new", "10.0.0.2:8080");
    let reqs = tr::reconcile(&before, &after).expect("reconcile");

    let remove_idx = reqs
        .iter()
        .position(|r| matches!(r.request_type, Some(RequestType::RemoveHttpsFrontend(_))))
        .expect("a RemoveHttpsFrontend for the old route");
    let add_idx = reqs
        .iter()
        .position(|r| matches!(r.request_type, Some(RequestType::AddHttpsFrontend(_))))
        .expect("an AddHttpsFrontend for the new route");
    assert!(
        remove_idx < add_idx,
        "old HTTPS frontend must be removed before the new one is added, \
         got remove at {remove_idx}, add at {add_idx}: {reqs:#?}"
    );
}

#[test]
fn ir_to_requests_with_filters() {
    let mut f = frontend(
        "app.example.com",
        ir::PathMatch::Prefix("/".into()),
        "app",
        false,
    );
    f.filters = ir::FrontendFilters {
        header_mods: vec![
            ir::HeaderMod {
                on: ir::HeaderTarget::Request,
                key: "X-Env".into(),
                value: Some("prod".into()),
            },
            ir::HeaderMod {
                on: ir::HeaderTarget::Response,
                key: "Server".into(),
                value: None, // delete
            },
        ],
        redirect: Some(ir::Redirect {
            scheme: Some(ir::Scheme::Https),
            status: ir::RedirectStatus::MovedPermanently,
        }),
        rewrite: Some(ir::Rewrite {
            hostname: Some("backend.svc".into()),
            path: Some("/v2".into()),
        }),
    };
    let model = ir::Ir {
        clusters: vec![cluster("app", ir::LbAlgorithm::RoundRobin, false)],
        backends: vec![backend("app", "10.0.0.1:8080", None)],
        frontends: vec![f],
        certificates: vec![],
        l4_frontends: vec![],
    };
    insta::assert_json_snapshot!(tr::ir_to_requests(&model));
}

#[test]
fn reconcile_add_new_route() {
    let small = ir::Ir {
        clusters: vec![cluster("app", ir::LbAlgorithm::RoundRobin, false)],
        backends: vec![backend("app", "10.0.0.1:8080", None)],
        frontends: vec![frontend(
            "app.example.com",
            ir::PathMatch::Prefix("/".into()),
            "app",
            false,
        )],
        certificates: vec![],
        l4_frontends: vec![],
    };
    insta::assert_json_snapshot!(tr::reconcile(&small, &sample_ir()).expect("reconcile"));
}

#[test]
fn reconcile_redirect_only_frontend_folds() {
    let mut f = frontend(
        "app.example.com",
        ir::PathMatch::Prefix("/".into()),
        "x",
        false,
    );
    f.cluster_id = None;
    f.filters.redirect = Some(ir::Redirect {
        scheme: Some(ir::Scheme::Https),
        status: ir::RedirectStatus::Found,
    });
    let model = ir::Ir {
        frontends: vec![f],
        ..Default::default()
    };
    let reqs = tr::reconcile(&ir::Ir::default(), &model).expect("redirect-only frontend must fold");
    assert_eq!(reqs.len(), 1);
}

#[test]
fn cluster_connection_limit_maps_to_request() {
    let mut c = cluster("app", ir::LbAlgorithm::RoundRobin, false);
    c.max_connections_per_ip = Some(100);
    c.retry_after = Some(30);
    let model = ir::Ir {
        clusters: vec![c],
        ..Default::default()
    };
    insta::assert_json_snapshot!(tr::ir_to_requests(&model));
}

#[test]
fn ir_to_requests_with_l4_tcp() {
    let model = ir::Ir {
        clusters: vec![cluster("pg", ir::LbAlgorithm::RoundRobin, false)],
        backends: vec![backend("pg", "10.0.0.1:5432", None)],
        l4_frontends: vec![ir::L4Frontend {
            protocol: ir::L4Protocol::Tcp,
            listener: addr("0.0.0.0:5432"),
            cluster_id: "pg".into(),
        }],
        ..Default::default()
    };
    insta::assert_json_snapshot!(tr::ir_to_requests(&model));
}

#[test]
fn reconcile_adds_then_removes_l4_route() {
    let model = ir::Ir {
        clusters: vec![cluster("pg", ir::LbAlgorithm::RoundRobin, false)],
        backends: vec![backend("pg", "10.0.0.1:5432", None)],
        l4_frontends: vec![ir::L4Frontend {
            protocol: ir::L4Protocol::Tcp,
            listener: addr("0.0.0.0:5432"),
            cluster_id: "pg".into(),
        }],
        ..Default::default()
    };
    // empty -> model: listener added + activated, plus the TCP frontend.
    let add = tr::reconcile(&ir::Ir::default(), &model).expect("reconcile add");
    assert!(add
        .iter()
        .any(|r| matches!(r.request_type, Some(RequestType::AddTcpListener(_)))));
    // ConfigState::diff emits the activation of a new active listener twice
    // (inline + trailing sweep); exactly one may survive the reconcile.
    assert_eq!(
        add.iter()
            .filter(|r| matches!(r.request_type, Some(RequestType::ActivateListener(_))))
            .count(),
        1,
        "a new listener must be activated exactly once: {add:#?}"
    );
    assert!(add
        .iter()
        .any(|r| matches!(r.request_type, Some(RequestType::AddTcpFrontend(_)))));
    insta::assert_json_snapshot!(add);

    // Re-applying the same state is a no-op.
    assert!(tr::reconcile(&model, &model).expect("idem").is_empty());

    // model -> empty: the frontend and listener are torn down, and the
    // listener is deactivated *before* it is removed (Sōzu's own teardown
    // order — pinned by explicit tiers, not by request-name sort order).
    let rm = tr::reconcile(&model, &ir::Ir::default()).expect("reconcile rm");
    assert!(rm
        .iter()
        .any(|r| matches!(r.request_type, Some(RequestType::RemoveTcpFrontend(_)))));
    let deactivate_idx = rm
        .iter()
        .position(|r| matches!(r.request_type, Some(RequestType::DeactivateListener(_))))
        .expect("a DeactivateListener for the torn-down listener");
    let remove_idx = rm
        .iter()
        .position(|r| matches!(r.request_type, Some(RequestType::RemoveListener(_))))
        .expect("a RemoveListener for the torn-down listener");
    assert!(
        deactivate_idx < remove_idx,
        "the listener must be deactivated before it is removed, \
         got deactivate at {deactivate_idx}, remove at {remove_idx}: {rm:#?}"
    );
}
