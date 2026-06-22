//! Golden-file tests for the Translator (IR -> Sōzu commands, and diffs).
//!
//! Snapshots are JSON of the emitted `Vec<Request>`. Determinism comes from the
//! Translator's canonical tier ordering; cert tests use real PEM fixtures
//! because `ConfigState` parses certificates (fingerprint + SNI names).

use std::net::SocketAddr;

use sozu_gw_ir as ir;
use sozu_gw_translator as tr;

const CERT_A: &str = include_str!("fixtures/cert_a.pem");
const KEY_A: &str = include_str!("fixtures/key_a.pem");
const CERT_B: &str = include_str!("fixtures/cert_b.pem");
const KEY_B: &str = include_str!("fixtures/key_b.pem");

fn addr(s: &str) -> SocketAddr {
    s.parse().expect("valid socket addr in test")
}

fn cluster(id: &str, lb: ir::LbAlgorithm, sticky: bool) -> ir::Cluster {
    ir::Cluster {
        id: id.to_string(),
        load_balancing: lb,
        sticky_session: sticky,
        https_redirect: false,
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
        cluster_id: cluster_id.to_string(),
        tls,
        listener: addr(if tls { "0.0.0.0:443" } else { "0.0.0.0:80" }),
    }
}

fn cert_a() -> ir::Certificate {
    ir::Certificate {
        listener: addr("0.0.0.0:443"),
        certificate: CERT_A.to_string(),
        chain: vec![],
        key: KEY_A.to_string(),
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
        certificates: vec![cert_a()],
    }
}

/// The empty data-plane state (no naming of `ConfigState` needed in tests).
fn empty_state() -> sozu_command_lib::state::ConfigState {
    tr::desired_state(&ir::Ir::default()).expect("empty IR folds")
}

#[test]
fn ir_to_requests_full() {
    insta::assert_json_snapshot!(tr::ir_to_requests(&sample_ir()));
}

#[test]
fn diff_from_empty_equals_full_adds() {
    let desired = tr::desired_state(&sample_ir()).expect("fold sample IR");
    insta::assert_json_snapshot!(tr::diff(&empty_state(), &desired));
}

#[test]
fn diff_is_idempotent() {
    let desired = tr::desired_state(&sample_ir()).expect("fold sample IR");
    assert!(
        tr::diff(&desired, &desired).is_empty(),
        "re-applying an unchanged state must emit no commands"
    );
}

#[test]
fn diff_scale_up_emits_single_add_backend() {
    let before = tr::desired_state(&sample_ir()).expect("fold");
    let mut ir2 = sample_ir();
    ir2.backends.push(backend("app", "10.0.0.3:8080", None));
    let after = tr::desired_state(&ir2).expect("fold");

    let reqs = tr::diff(&before, &after);
    assert_eq!(reqs.len(), 1, "scale-up should be exactly one request");
    insta::assert_json_snapshot!(reqs);
}

#[test]
fn diff_scale_down_emits_single_remove_backend() {
    let before = tr::desired_state(&sample_ir()).expect("fold");
    let mut ir2 = sample_ir();
    ir2.backends.retain(|b| b.address != addr("10.0.0.2:8080"));
    let after = tr::desired_state(&ir2).expect("fold");

    let reqs = tr::diff(&before, &after);
    assert_eq!(reqs.len(), 1, "scale-down should be exactly one request");
    insta::assert_json_snapshot!(reqs);
}

#[test]
fn diff_cert_rotation_adds_new_before_removing_old() {
    use sozu_command_lib::proto::command::request::RequestType;

    let before = tr::desired_state(&sample_ir()).expect("fold");
    let mut ir2 = sample_ir();
    ir2.certificates = vec![ir::Certificate {
        certificate: CERT_B.to_string(),
        key: KEY_B.to_string(),
        ..cert_a()
    }];
    let after = tr::desired_state(&ir2).expect("fold");

    let reqs = tr::diff(&before, &after);

    let add_pos = reqs
        .iter()
        .position(|r| matches!(r.request_type, Some(RequestType::AddCertificate(_))));
    let remove_pos = reqs
        .iter()
        .position(|r| matches!(r.request_type, Some(RequestType::RemoveCertificate(_))));
    assert!(add_pos.is_some(), "rotation must add the new certificate");
    assert!(
        remove_pos.is_some(),
        "rotation must remove the old certificate"
    );
    assert!(
        add_pos < remove_pos,
        "the new certificate must be added before the old one is removed (no TLS gap)"
    );
    insta::assert_json_snapshot!(reqs);
}

#[test]
fn diff_add_new_route() {
    // Start from just the `app` HTTP route, then add the full sample.
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
    };
    let before = tr::desired_state(&small).expect("fold small");
    let after = tr::desired_state(&sample_ir()).expect("fold full");
    insta::assert_json_snapshot!(tr::diff(&before, &after));
}
