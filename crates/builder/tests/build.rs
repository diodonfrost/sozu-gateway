//! Golden + behavioural tests for the Builder (K8s objects -> IR).

use std::collections::BTreeMap;

use k8s_openapi::api::core::v1::{Secret, Service};
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
