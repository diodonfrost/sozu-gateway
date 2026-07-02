//! Golden + behavioural tests for the Builder (K8s objects -> IR).

use std::collections::BTreeMap;

use k8s_openapi::api::core::v1::{ConfigMap, Secret, Service};
use k8s_openapi::api::discovery::v1::EndpointSlice;
use k8s_openapi::api::networking::v1::Ingress;
use k8s_openapi::apimachinery::pkg::apis::meta::v1::ObjectMeta;
use k8s_openapi::ByteString;
use serde_json::json;

use sozu_gw_builder::{build, BuildConfig, Inputs, Problem};
use sozu_gw_ir as ir;

const CERT_A: &str = include_str!("fixtures/cert_a.pem");
const KEY_A: &str = include_str!("fixtures/key_a.pem");

fn from_json<T: serde::de::DeserializeOwned>(v: serde_json::Value) -> T {
    serde_json::from_value(v).expect("valid k8s object json")
}

fn tls_secret(ns: &str, name: &str, crt: &str, key: &str) -> Secret {
    let mut data = BTreeMap::new();
    data.insert("tls.crt".to_string(), ByteString(crt.as_bytes().to_vec()));
    data.insert("tls.key".to_string(), ByteString(key.as_bytes().to_vec()));
    Secret {
        metadata: ObjectMeta {
            name: Some(name.to_string()),
            namespace: Some(ns.to_string()),
            ..Default::default()
        },
        data: Some(data),
        type_: Some("kubernetes.io/tls".to_string()),
        ..Default::default()
    }
}

/// Service `web` in `demo`: port 80 (name "http") -> targetPort 8080.
fn web_service() -> Service {
    from_json(json!({
        "apiVersion": "v1", "kind": "Service",
        "metadata": { "name": "web", "namespace": "demo" },
        "spec": { "ports": [{ "name": "http", "port": 80, "targetPort": 8080 }] }
    }))
}

/// EndpointSlice for `web` with two ready endpoints + one not-ready (excluded).
fn web_slice() -> EndpointSlice {
    from_json(json!({
        "apiVersion": "discovery.k8s.io/v1", "kind": "EndpointSlice",
        "metadata": {
            "name": "web-abc", "namespace": "demo",
            "labels": { "kubernetes.io/service-name": "web" }
        },
        "addressType": "IPv4",
        "ports": [{ "name": "http", "port": 8080 }],
        "endpoints": [
            { "addresses": ["10.244.0.5"], "conditions": { "ready": true } },
            { "addresses": ["10.244.0.6"], "conditions": { "ready": true } },
            { "addresses": ["10.244.0.7"], "conditions": { "ready": false } }
        ]
    }))
}

fn ingress_tls() -> Ingress {
    from_json(json!({
        "apiVersion": "networking.k8s.io/v1", "kind": "Ingress",
        "metadata": { "name": "web", "namespace": "demo" },
        "spec": {
            "ingressClassName": "sozu",
            "tls": [{ "hosts": ["app.example.com"], "secretName": "app-tls" }],
            "rules": [{
                "host": "app.example.com",
                "http": { "paths": [
                    { "path": "/", "pathType": "Prefix",
                      "backend": { "service": { "name": "web", "port": { "number": 80 } } } }
                ]}
            }]
        }
    }))
}

#[test]
fn happy_path_http_and_tls() {
    let inputs = Inputs {
        ingresses: vec![ingress_tls()],
        services: vec![web_service()],
        endpointslices: vec![web_slice()],
        secrets: vec![tls_secret("demo", "app-tls", CERT_A, KEY_A)],
        ..Default::default()
    };
    let out = build(&BuildConfig::default(), &inputs);

    // 1 cluster, 2 ready backends, http + https frontends, 1 cert, accepted clean.
    assert_eq!(out.ir.clusters.len(), 1);
    assert_eq!(out.ir.backends.len(), 2);
    assert_eq!(out.ir.frontends.len(), 2);
    assert_eq!(out.ir.certificates.len(), 1);
    assert_eq!(out.results.len(), 1);
    assert!(out.results[0].problems.is_empty(), "{:?}", out.results[0]);

    insta::assert_json_snapshot!(out);
}

#[test]
fn ignores_other_ingress_class() {
    let mut ing: Ingress = ingress_tls();
    ing.spec.as_mut().unwrap().ingress_class_name = Some("nginx".to_string());
    let inputs = Inputs {
        ingresses: vec![ing],
        services: vec![web_service()],
        endpointslices: vec![web_slice()],
        secrets: vec![],
        ..Default::default()
    };
    let out = build(&BuildConfig::default(), &inputs);
    assert!(out.ir.clusters.is_empty());
    assert!(
        out.results.is_empty(),
        "non-ours ingress must not appear in results"
    );
}

#[test]
fn missing_secret_reports_problem_and_skips_tls() {
    let inputs = Inputs {
        ingresses: vec![ingress_tls()],
        services: vec![web_service()],
        endpointslices: vec![web_slice()],
        secrets: vec![], // secret absent
        ..Default::default()
    };
    let out = build(&BuildConfig::default(), &inputs);
    assert_eq!(out.ir.certificates.len(), 0, "no cert without the secret");
    assert_eq!(out.ir.frontends.len(), 1, "only the HTTP frontend remains");
    assert_eq!(
        out.results[0].problems,
        vec![Problem::SecretNotFound {
            secret: "app-tls".to_string()
        }]
    );
}

#[test]
fn service_not_found_reports_problem() {
    let inputs = Inputs {
        ingresses: vec![ingress_tls()],
        services: vec![], // service absent
        endpointslices: vec![],
        secrets: vec![tls_secret("demo", "app-tls", CERT_A, KEY_A)],
        ..Default::default()
    };
    let out = build(&BuildConfig::default(), &inputs);
    assert!(out.ir.clusters.is_empty());
    assert!(out.ir.frontends.is_empty());
    assert_eq!(
        out.results[0].problems,
        vec![Problem::ServiceNotFound {
            service: "web".to_string()
        }]
    );
}

#[test]
fn no_ready_endpoints_keeps_cluster_reports_problem() {
    let slice: EndpointSlice = from_json(json!({
        "apiVersion": "discovery.k8s.io/v1", "kind": "EndpointSlice",
        "metadata": { "name": "web-x", "namespace": "demo",
            "labels": { "kubernetes.io/service-name": "web" } },
        "addressType": "IPv4",
        "ports": [{ "name": "http", "port": 8080 }],
        "endpoints": [{ "addresses": ["10.244.0.9"], "conditions": { "ready": false } }]
    }));
    let inputs = Inputs {
        ingresses: vec![ingress_tls()],
        services: vec![web_service()],
        endpointslices: vec![slice],
        secrets: vec![tls_secret("demo", "app-tls", CERT_A, KEY_A)],
        ..Default::default()
    };
    let out = build(&BuildConfig::default(), &inputs);
    assert_eq!(out.ir.clusters.len(), 1, "cluster is still declared");
    assert_eq!(out.ir.backends.len(), 0);
    assert!(out.results[0]
        .problems
        .contains(&Problem::NoReadyEndpoints {
            service: "web".to_string()
        }));
}

#[test]
fn path_types_map_correctly() {
    let ing: Ingress = from_json(json!({
        "apiVersion": "networking.k8s.io/v1", "kind": "Ingress",
        "metadata": { "name": "paths", "namespace": "demo" },
        "spec": {
            "ingressClassName": "sozu",
            "rules": [{
                "host": "app.example.com",
                "http": { "paths": [
                    { "path": "/exact", "pathType": "Exact",
                      "backend": { "service": { "name": "web", "port": { "name": "http" } } } },
                    { "path": "/regex.*", "pathType": "ImplementationSpecific",
                      "backend": { "service": { "name": "web", "port": { "name": "http" } } } }
                ]}
            }]
        }
    }));
    let inputs = Inputs {
        ingresses: vec![ing],
        services: vec![web_service()],
        endpointslices: vec![web_slice()],
        secrets: vec![],
        ..Default::default()
    };
    let out = build(&BuildConfig::default(), &inputs);
    // Two HTTP frontends (Exact + Regex), resolved via the named service port.
    assert_eq!(out.ir.frontends.len(), 2);
    insta::assert_json_snapshot!(out.ir.frontends);
}

/// `web` Service carrying the load-balancing + sticky-session annotations.
fn annotated_service(lb: &str, sticky: &str) -> Service {
    from_json(json!({
        "apiVersion": "v1", "kind": "Service",
        "metadata": { "name": "web", "namespace": "demo",
            "annotations": {
                "sozu.io/load-balancing": lb,
                "sozu.io/sticky-sessions": sticky,
            } },
        "spec": { "ports": [{ "name": "http", "port": 80, "targetPort": 8080 }] }
    }))
}

#[test]
fn service_annotations_set_cluster_lb_and_sticky() {
    let inputs = Inputs {
        ingresses: vec![ingress_tls()],
        services: vec![annotated_service("least-loaded", "true")],
        endpointslices: vec![web_slice()],
        secrets: vec![tls_secret("demo", "app-tls", CERT_A, KEY_A)],
        ..Default::default()
    };
    let out = build(&BuildConfig::default(), &inputs);
    assert_eq!(out.ir.clusters.len(), 1);
    assert!(matches!(
        out.ir.clusters[0].load_balancing,
        ir::LbAlgorithm::LeastLoaded
    ));
    assert!(out.ir.clusters[0].sticky_session);
}

#[test]
fn lb_annotation_is_normalised_and_unknown_defaults_to_round_robin() {
    // Spacing/underscores/case are normalised; an unknown value is not an error,
    // it just keeps the round-robin default.
    let cases = [
        ("Power_Of Two", true), // -> PowerOfTwo
        ("bogus", false),       // -> RoundRobin (unknown)
    ];
    for (value, is_p2c) in cases {
        let inputs = Inputs {
            ingresses: vec![ingress_tls()],
            services: vec![annotated_service(value, "false")],
            endpointslices: vec![web_slice()],
            secrets: vec![tls_secret("demo", "app-tls", CERT_A, KEY_A)],
            ..Default::default()
        };
        let out = build(&BuildConfig::default(), &inputs);
        let lb = &out.ir.clusters[0].load_balancing;
        if is_p2c {
            assert!(matches!(lb, ir::LbAlgorithm::PowerOfTwo), "value={value:?}");
        } else {
            assert!(matches!(lb, ir::LbAlgorithm::RoundRobin), "value={value:?}");
        }
        assert!(!out.ir.clusters[0].sticky_session);
    }
}

#[test]
fn no_annotations_keep_round_robin_no_sticky() {
    let inputs = Inputs {
        ingresses: vec![ingress_tls()],
        services: vec![web_service()],
        endpointslices: vec![web_slice()],
        secrets: vec![tls_secret("demo", "app-tls", CERT_A, KEY_A)],
        ..Default::default()
    };
    let out = build(&BuildConfig::default(), &inputs);
    assert!(matches!(
        out.ir.clusters[0].load_balancing,
        ir::LbAlgorithm::RoundRobin
    ));
    assert!(!out.ir.clusters[0].sticky_session);
}

#[test]
fn service_annotations_set_connection_limit_per_ip() {
    let svc: Service = from_json(json!({
        "apiVersion": "v1", "kind": "Service",
        "metadata": { "name": "web", "namespace": "demo",
            "annotations": {
                "sozu.io/max-connections-per-ip": "100",
                "sozu.io/retry-after": "30",
            } },
        "spec": { "ports": [{ "name": "http", "port": 80, "targetPort": 8080 }] }
    }));
    let inputs = Inputs {
        ingresses: vec![ingress_tls()],
        services: vec![svc],
        endpointslices: vec![web_slice()],
        secrets: vec![tls_secret("demo", "app-tls", CERT_A, KEY_A)],
        ..Default::default()
    };
    let out = build(&BuildConfig::default(), &inputs);
    assert_eq!(out.ir.clusters[0].max_connections_per_ip, Some(100));
    assert_eq!(out.ir.clusters[0].retry_after, Some(30));
}

#[test]
fn non_numeric_connection_limit_is_ignored() {
    // A typo'd value falls back to the global default rather than failing.
    let svc: Service = from_json(json!({
        "apiVersion": "v1", "kind": "Service",
        "metadata": { "name": "web", "namespace": "demo",
            "annotations": { "sozu.io/max-connections-per-ip": "lots" } },
        "spec": { "ports": [{ "name": "http", "port": 80, "targetPort": 8080 }] }
    }));
    let inputs = Inputs {
        ingresses: vec![ingress_tls()],
        services: vec![svc],
        endpointslices: vec![web_slice()],
        secrets: vec![tls_secret("demo", "app-tls", CERT_A, KEY_A)],
        ..Default::default()
    };
    let out = build(&BuildConfig::default(), &inputs);
    assert_eq!(out.ir.clusters[0].max_connections_per_ip, None);
    assert_eq!(out.ir.clusters[0].retry_after, None);
}

#[test]
fn tls_ingress_redirects_http_to_https_by_default() {
    let inputs = Inputs {
        ingresses: vec![ingress_tls()],
        services: vec![web_service()],
        endpointslices: vec![web_slice()],
        secrets: vec![tls_secret("demo", "app-tls", CERT_A, KEY_A)],
        ..Default::default()
    };
    let out = build(&BuildConfig::default(), &inputs);

    // Sorted (tls, host, cluster): [0] = HTTP frontend, [1] = HTTPS frontend.
    let http = &out.ir.frontends[0];
    let https = &out.ir.frontends[1];
    assert!(!http.tls);
    assert!(https.tls);
    let r = http
        .filters
        .redirect
        .as_ref()
        .expect("HTTP frontend redirects");
    assert!(matches!(r.scheme, Some(ir::Scheme::Https)));
    assert!(matches!(r.status, ir::RedirectStatus::MovedPermanently));
    assert!(
        https.filters.redirect.is_none(),
        "the HTTPS frontend must serve, not redirect"
    );
}

#[test]
fn ssl_redirect_can_be_opted_out() {
    let mut ing = ingress_tls();
    ing.metadata.annotations = Some(
        [("sozu.io/ssl-redirect".to_string(), "false".to_string())]
            .into_iter()
            .collect(),
    );
    let inputs = Inputs {
        ingresses: vec![ing],
        services: vec![web_service()],
        endpointslices: vec![web_slice()],
        secrets: vec![tls_secret("demo", "app-tls", CERT_A, KEY_A)],
        ..Default::default()
    };
    let out = build(&BuildConfig::default(), &inputs);
    assert!(
        out.ir
            .frontends
            .iter()
            .all(|f| f.filters.redirect.is_none()),
        "opt-out disables the auto HTTP→HTTPS redirect"
    );
}

#[test]
fn http_only_ingress_is_not_redirected() {
    // No TLS on this Ingress -> nothing to redirect to, so it keeps serving HTTP.
    let inputs = Inputs {
        ingresses: vec![ingress_tls()], // reuse, but omit the secret below
        services: vec![web_service()],
        endpointslices: vec![web_slice()],
        secrets: vec![], // cert never loads -> host is not TLS-ready
        ..Default::default()
    };
    let out = build(&BuildConfig::default(), &inputs);
    assert_eq!(out.ir.frontends.len(), 1, "only the HTTP frontend");
    assert!(out.ir.frontends[0].filters.redirect.is_none());
}

/// A build with the given Secret material must report `InvalidCertificate`,
/// load no cert, and keep the host's frontend plain HTTP (no HTTPS, no
/// redirect) — the "TLS-ready only with a successfully loaded cert" rule.
fn assert_invalid_certificate(crt: &str, key: &str) {
    let inputs = Inputs {
        ingresses: vec![ingress_tls()],
        services: vec![web_service()],
        endpointslices: vec![web_slice()],
        secrets: vec![tls_secret("demo", "app-tls", crt, key)],
        ..Default::default()
    };
    let out = build(&BuildConfig::default(), &inputs);

    assert_eq!(out.ir.certificates.len(), 0, "invalid cert must not load");
    assert_eq!(out.ir.frontends.len(), 1, "only the HTTP frontend remains");
    assert!(!out.ir.frontends[0].tls);
    assert!(
        out.ir.frontends[0].filters.redirect.is_none(),
        "no HTTPS to redirect to"
    );
    assert_eq!(out.results.len(), 1, "build still succeeds");
    assert!(
        matches!(
            &out.results[0].problems[..],
            [Problem::InvalidCertificate { secret, .. }] if secret == "app-tls"
        ),
        "expected InvalidCertificate, got {:?}",
        out.results[0].problems
    );
}

#[test]
fn cert_with_valid_markers_but_garbage_base64_is_rejected() {
    // split_certificate_chain is purely textual: this passes the marker scan
    // but the body is not base64. It must be reported per-Secret, not crash
    // the translator's diff downstream.
    let garbage = "-----BEGIN CERTIFICATE-----\nnot!!base64@@data\n-----END CERTIFICATE-----\n";
    assert_invalid_certificate(garbage, KEY_A);
}

#[test]
fn cert_with_valid_base64_but_non_der_body_is_rejected() {
    // The body IS valid base64 (so a decode-and-hash check alone would pass)
    // but the decoded bytes are not DER. Sōzu parses the X509 when the
    // AddCertificate is applied and rejects it, aborting the whole apply
    // batch every cycle — so the builder must reject it up front.
    let garbage = "-----BEGIN CERTIFICATE-----\n\
                   bm90IGEgY2VydGlmaWNhdGUsIGp1c3QgYmFzZTY0IGdhcmJhZ2U=\n\
                   -----END CERTIFICATE-----\n";
    assert_invalid_certificate(garbage, KEY_A);
}

#[test]
fn chain_cert_with_non_der_body_is_rejected() {
    // A valid leaf followed by a valid-base64/non-DER intermediate: the chain
    // rides in the same AddCertificate, so it must be validated too.
    let garbage_chain = format!(
        "{CERT_A}-----BEGIN CERTIFICATE-----\n\
         bm90IGEgY2VydGlmaWNhdGUsIGp1c3QgYmFzZTY0IGdhcmJhZ2U=\n\
         -----END CERTIFICATE-----\n"
    );
    assert_invalid_certificate(&garbage_chain, KEY_A);
}

#[test]
fn cert_with_garbage_key_is_rejected() {
    // A parseable cert with a corrupt key would be rejected by Sōzu at
    // AddCertificate time, blocking every frontend add (certs tier first).
    let garbage = "-----BEGIN PRIVATE KEY-----\nnot!!base64@@data\n-----END PRIVATE KEY-----\n";
    assert_invalid_certificate(CERT_A, garbage);
}

#[test]
fn key_with_non_key_pem_label_is_rejected() {
    // A well-formed PEM block that is not a private key (here: a certificate)
    // is not plausible tls.key material.
    assert_invalid_certificate(CERT_A, CERT_A);
}

#[test]
fn certs_sharing_a_secret_are_merged_with_unioned_names() {
    // One TLS Secret backing two hosts (here two TLS entries on one Ingress, the
    // same shape as an Ingress + a Gateway listener sharing a Secret) must yield
    // ONE certificate with both names — Sōzu keys a cert by (listener, fp), so a
    // second entry would make the translator ReplaceCertificate forever.
    let ing: Ingress = from_json(json!({
        "apiVersion": "networking.k8s.io/v1", "kind": "Ingress",
        "metadata": { "name": "web", "namespace": "demo" },
        "spec": {
            "ingressClassName": "sozu",
            "tls": [
                { "hosts": ["b.example.com"], "secretName": "app-tls" },
                { "hosts": ["a.example.com"], "secretName": "app-tls" }
            ],
            "rules": [
                { "host": "a.example.com", "http": { "paths": [
                    { "path": "/", "pathType": "Prefix",
                      "backend": { "service": { "name": "web", "port": { "number": 80 } } } } ] } },
                { "host": "b.example.com", "http": { "paths": [
                    { "path": "/", "pathType": "Prefix",
                      "backend": { "service": { "name": "web", "port": { "number": 80 } } } } ] } }
            ]
        }
    }));
    let inputs = Inputs {
        ingresses: vec![ing],
        services: vec![web_service()],
        endpointslices: vec![web_slice()],
        secrets: vec![tls_secret("demo", "app-tls", CERT_A, KEY_A)],
        ..Default::default()
    };
    let out = build(&BuildConfig::default(), &inputs);
    assert_eq!(
        out.ir.certificates.len(),
        1,
        "one cert per (listener, fingerprint)"
    );
    assert_eq!(
        out.ir.certificates[0].names,
        vec!["a.example.com".to_string(), "b.example.com".to_string()],
        "names unioned and sorted"
    );
}

#[test]
fn same_der_cert_with_different_pem_wrapping_is_merged() {
    // Re-encode CERT_A's base64 body at a different line width: same DER (so
    // the same fingerprint — Sōzu's identity), byte-different PEM text. This
    // is the cert-manager vs hand-made Secret shape. The two entries must
    // merge into ONE certificate with the unioned names, or the translator
    // would churn ReplaceCertificate forever and one hostname would lose TLS.
    let body: String = CERT_A.lines().filter(|l| !l.starts_with("-----")).collect();
    let mut rewrapped = String::from("-----BEGIN CERTIFICATE-----\n");
    for chunk in body.as_bytes().chunks(48) {
        rewrapped.push_str(std::str::from_utf8(chunk).expect("ascii base64"));
        rewrapped.push('\n');
    }
    rewrapped.push_str("-----END CERTIFICATE-----\n");
    assert_ne!(rewrapped, CERT_A, "the PEM texts must differ");

    let ingress = |name: &str, host: &str, secret: &str| -> Ingress {
        from_json(json!({
            "apiVersion": "networking.k8s.io/v1", "kind": "Ingress",
            "metadata": { "name": name, "namespace": "demo" },
            "spec": {
                "ingressClassName": "sozu",
                "tls": [{ "hosts": [host], "secretName": secret }],
                "rules": [{
                    "host": host,
                    "http": { "paths": [
                        { "path": "/", "pathType": "Prefix",
                          "backend": { "service": { "name": "web", "port": { "number": 80 } } } }
                    ]}
                }]
            }
        }))
    };
    let inputs = Inputs {
        ingresses: vec![
            ingress("a", "a.example.com", "tls-a"),
            ingress("b", "b.example.com", "tls-b"),
        ],
        services: vec![web_service()],
        endpointslices: vec![web_slice()],
        secrets: vec![
            tls_secret("demo", "tls-a", CERT_A, KEY_A),
            tls_secret("demo", "tls-b", &rewrapped, KEY_A),
        ],
        ..Default::default()
    };
    let out = build(&BuildConfig::default(), &inputs);

    assert_eq!(
        out.ir.certificates.len(),
        1,
        "one cert per (listener, fingerprint), regardless of PEM wrapping"
    );
    assert_eq!(
        out.ir.certificates[0].names,
        vec!["a.example.com".to_string(), "b.example.com".to_string()],
        "names unioned across both Secrets"
    );
}

/// Service `web` + ready EndpointSlice in an arbitrary namespace.
fn web_service_in(ns: &str) -> (Service, EndpointSlice) {
    let svc = from_json(json!({
        "apiVersion": "v1", "kind": "Service",
        "metadata": { "name": "web", "namespace": ns },
        "spec": { "ports": [{ "name": "http", "port": 80, "targetPort": 8080 }] }
    }));
    let slice = from_json(json!({
        "apiVersion": "discovery.k8s.io/v1", "kind": "EndpointSlice",
        "metadata": { "name": "web-abc", "namespace": ns,
            "labels": { "kubernetes.io/service-name": "web" } },
        "addressType": "IPv4",
        "ports": [{ "name": "http", "port": 8080 }],
        "endpoints": [{ "addresses": ["10.244.0.5"], "conditions": { "ready": true } }]
    }));
    (svc, slice)
}

/// Plain-HTTP Ingress `<ns>/<name>` routing `host` `/` to `<ns>/web:80`.
fn plain_ingress(ns: &str, name: &str, host: &str) -> Ingress {
    from_json(json!({
        "apiVersion": "networking.k8s.io/v1", "kind": "Ingress",
        "metadata": { "name": name, "namespace": ns },
        "spec": {
            "ingressClassName": "sozu",
            "rules": [{
                "host": host,
                "http": { "paths": [
                    { "path": "/", "pathType": "Prefix",
                      "backend": { "service": { "name": "web", "port": { "number": 80 } } } }
                ]}
            }]
        }
    }))
}

#[test]
fn cross_namespace_host_path_collision_is_reported_on_the_loser() {
    // Two Ingresses in different namespaces claim the same host+path with
    // different Services. Sōzu keys the route by host+path (not by cluster),
    // so only one can win — the winner must be the one the translator's dedup
    // already kept (smallest cluster id), and the loser must SEE the theft
    // instead of both owners reading accepted-with-no-problems.
    let (svc_a, slice_a) = web_service_in("aaa");
    let (svc_b, slice_b) = web_service_in("bbb");
    let inputs = Inputs {
        ingresses: vec![
            plain_ingress("bbb", "web", "clash.example.com"),
            plain_ingress("aaa", "web", "clash.example.com"),
        ],
        services: vec![svc_a, svc_b],
        endpointslices: vec![slice_a, slice_b],
        ..Default::default()
    };
    let out = build(&BuildConfig::default(), &inputs);

    assert_eq!(out.ir.frontends.len(), 1, "one frontend per route key");
    assert_eq!(
        out.ir.frontends[0].cluster_id.as_deref(),
        Some("aaa.web.80"),
        "the translator's winner (smallest cluster id) is kept"
    );
    let winner = out
        .results
        .iter()
        .find(|r| r.namespace == "aaa")
        .expect("aaa result");
    assert!(winner.problems.is_empty(), "{winner:?}");
    let loser = out
        .results
        .iter()
        .find(|r| r.namespace == "bbb")
        .expect("bbb result");
    assert_eq!(
        loser.problems,
        vec![Problem::RouteCollision {
            hostname: "clash.example.com".to_string(),
            path: "/".to_string(),
            winner: "aaa.web.80".to_string(),
        }]
    );
}

#[test]
fn identical_duplicate_routes_are_benign_and_unreported() {
    // Two Ingresses claiming the same host+path with the SAME Service and the
    // same filters are a harmless overlap: one frontend, zero problems.
    let inputs = Inputs {
        ingresses: vec![
            plain_ingress("demo", "ing-a", "app.example.com"),
            plain_ingress("demo", "ing-b", "app.example.com"),
        ],
        services: vec![web_service()],
        endpointslices: vec![web_slice()],
        ..Default::default()
    };
    let out = build(&BuildConfig::default(), &inputs);

    assert_eq!(out.ir.frontends.len(), 1, "deduped to one frontend");
    for r in &out.results {
        assert!(r.problems.is_empty(), "{r:?}");
    }
}

#[test]
fn tcp_services_configmap_maps_to_l4_frontend() {
    let cm: ConfigMap = from_json(json!({
        "apiVersion": "v1", "kind": "ConfigMap",
        "metadata": { "name": "tcp-services", "namespace": "sozu-system" },
        "data": {
            "5432": "demo/web:80",   // valid -> one L4 frontend + pod-IP backends
            "80": "demo/web:80",     // reserved (HTTP listener) -> reported
            "oops": "not-a-mapping"  // unparseable -> reported
        }
    }));
    let inputs = Inputs {
        services: vec![web_service()],
        endpointslices: vec![web_slice()],
        tcp_services: Some(cm),
        ..Default::default()
    };
    let out = build(&BuildConfig::default(), &inputs);

    assert_eq!(
        out.ir.l4_frontends.len(),
        1,
        "only the valid mapping yields a route"
    );
    let f = &out.ir.l4_frontends[0];
    assert!(matches!(f.protocol, ir::L4Protocol::Tcp));
    assert_eq!(f.listener.to_string(), "0.0.0.0:5432");
    assert_eq!(out.ir.backends.len(), 2, "L4 service resolved to pod IPs");

    let problems: Vec<&Problem> = out.l4_results.iter().flat_map(|r| &r.problems).collect();
    assert!(problems
        .iter()
        .any(|p| matches!(p, Problem::L4PortReserved { port: 80 })));
    assert!(problems
        .iter()
        .any(|p| matches!(p, Problem::InvalidL4Mapping { .. })));
}

#[test]
fn default_backend_only_ingress_reports_unsupported() {
    // spec.defaultBackend has no verified Sōzu mapping: an Ingress made of
    // only a defaultBackend builds to nothing, so the owner must see WHY
    // instead of accepted-with-no-problems while requests 404.
    let ing: Ingress = from_json(json!({
        "apiVersion": "networking.k8s.io/v1", "kind": "Ingress",
        "metadata": { "name": "web", "namespace": "demo" },
        "spec": {
            "ingressClassName": "sozu",
            "defaultBackend": { "service": { "name": "web", "port": { "number": 80 } } }
        }
    }));
    let inputs = Inputs {
        ingresses: vec![ing],
        services: vec![web_service()],
        endpointslices: vec![web_slice()],
        ..Default::default()
    };
    let out = build(&BuildConfig::default(), &inputs);

    assert!(out.ir.frontends.is_empty(), "defaultBackend is not routed");
    assert_eq!(
        out.results[0].problems,
        vec![Problem::DefaultBackendUnsupported]
    );
}

#[test]
fn default_backend_next_to_rules_still_builds_the_rules() {
    let mut ing = ingress_tls();
    ing.spec.as_mut().expect("spec").default_backend = Some(from_json(json!({
        "service": { "name": "web", "port": { "number": 80 } }
    })));
    let inputs = Inputs {
        ingresses: vec![ing],
        services: vec![web_service()],
        endpointslices: vec![web_slice()],
        secrets: vec![tls_secret("demo", "app-tls", CERT_A, KEY_A)],
        ..Default::default()
    };
    let out = build(&BuildConfig::default(), &inputs);

    assert_eq!(out.ir.frontends.len(), 2, "the rules translate as usual");
    assert!(out.results[0]
        .problems
        .contains(&Problem::DefaultBackendUnsupported));
}

#[test]
fn ingress_hostless_rule_maps_to_catch_all() {
    // An Ingress rule with no `host` is a catch-all: emit one plain-HTTP `*`
    // frontend (Sōzu DomainRule::Any), no HTTPS frontend, no cert, no problem.
    let ing: Ingress = from_json(json!({
        "apiVersion": "networking.k8s.io/v1", "kind": "Ingress",
        "metadata": { "name": "web", "namespace": "demo" },
        "spec": {
            "ingressClassName": "sozu",
            "rules": [{
                "http": { "paths": [
                    { "path": "/", "pathType": "Prefix",
                      "backend": { "service": { "name": "web", "port": { "number": 80 } } } }
                ]}
            }]
        }
    }));
    let inputs = Inputs {
        ingresses: vec![ing],
        services: vec![web_service()],
        endpointslices: vec![web_slice()],
        ..Default::default()
    };
    let out = build(&BuildConfig::default(), &inputs);

    assert_eq!(
        out.ir.frontends.len(),
        1,
        "only the HTTP catch-all frontend"
    );
    assert_eq!(out.ir.frontends[0].hostname, "*");
    assert!(!out.ir.frontends[0].tls);
    assert_eq!(out.ir.certificates.len(), 0);
    assert!(out.results[0].problems.is_empty(), "{:?}", out.results[0]);
}

#[test]
fn problem_display_carries_the_detail_and_reason_stays_machine_readable() {
    let p = Problem::ServicePortNotFound {
        service: "demo/web".into(),
        port: "http".into(),
    };
    assert!(p.to_string().contains("demo/web") && p.to_string().contains("http"));
    assert_eq!(p.reason(), "ServicePortNotFound");

    let p = Problem::ListenerPortMismatch {
        listener: "https".into(),
        declared: 8443,
        expected: 443,
    };
    assert!(p.to_string().contains("8443") && p.to_string().contains("443"));
    assert_eq!(
        p.listener(),
        Some("https"),
        "listener-scoped variants say so"
    );
    assert_eq!(Problem::TimeoutsUnsupported.listener(), None);
}
