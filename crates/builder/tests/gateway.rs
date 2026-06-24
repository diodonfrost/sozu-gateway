//! Behavioural tests for the Gateway API -> IR mapping (Phase 2).

use std::collections::BTreeMap;

use k8s_openapi::api::core::v1::{Secret, Service};
use k8s_openapi::api::discovery::v1::EndpointSlice;
use k8s_openapi::api::networking::v1::Ingress;
use k8s_openapi::apimachinery::pkg::apis::meta::v1::ObjectMeta;
use k8s_openapi::ByteString;
use serde_json::json;

use sozu_gw_builder::{build, BuildConfig, Inputs, Problem};
use sozu_gw_gateway_api::{Gateway, GatewayClass, HttpRoute};
use sozu_gw_ir as ir;

const CERT_A: &str = include_str!("fixtures/cert_a.pem");
const KEY_A: &str = include_str!("fixtures/key_a.pem");

fn from_json<T: serde::de::DeserializeOwned>(v: serde_json::Value) -> T {
    serde_json::from_value(v).expect("valid k8s object json")
}

fn web_service() -> Service {
    from_json(json!({
        "metadata": { "name": "web", "namespace": "demo" },
        "spec": { "ports": [{ "name": "http", "port": 80, "targetPort": 8080 }] }
    }))
}

fn web_slice() -> EndpointSlice {
    from_json(json!({
        "metadata": { "name": "web-1", "namespace": "demo",
            "labels": { "kubernetes.io/service-name": "web" } },
        "addressType": "IPv4",
        "ports": [{ "name": "http", "port": 8080 }],
        "endpoints": [
            { "addresses": ["10.244.0.5"], "conditions": { "ready": true } },
            { "addresses": ["10.244.0.6"], "conditions": { "ready": true } }
        ]
    }))
}

fn tls_secret() -> Secret {
    let mut data = BTreeMap::new();
    data.insert(
        "tls.crt".to_string(),
        ByteString(CERT_A.as_bytes().to_vec()),
    );
    data.insert("tls.key".to_string(), ByteString(KEY_A.as_bytes().to_vec()));
    Secret {
        metadata: ObjectMeta {
            name: Some("app-tls".to_string()),
            namespace: Some("demo".to_string()),
            ..Default::default()
        },
        data: Some(data),
        type_: Some("kubernetes.io/tls".to_string()),
        ..Default::default()
    }
}

fn gateway_class(controller: &str) -> GatewayClass {
    from_json(json!({
        "metadata": { "name": "sozu" },
        "spec": { "controllerName": controller }
    }))
}

fn http_gateway() -> Gateway {
    from_json(json!({
        "metadata": { "name": "gw", "namespace": "demo" },
        "spec": { "gatewayClassName": "sozu",
            "listeners": [{ "name": "http", "protocol": "HTTP", "port": 80 }] }
    }))
}

fn https_gateway() -> Gateway {
    from_json(json!({
        "metadata": { "name": "gw", "namespace": "demo" },
        "spec": { "gatewayClassName": "sozu", "listeners": [{
            "name": "https", "protocol": "HTTPS", "port": 443,
            "hostname": "app.example.com",
            "tls": { "mode": "Terminate", "certificateRefs": [{ "name": "app-tls" }] }
        }]}
    }))
}

/// HTTPRoute to `web:80` with one prefix match. `extra_backend` adds a second
/// backendRef (to exercise the unsupported weighted-split path).
fn route_to_web(extra_backend: bool) -> HttpRoute {
    let mut backend_refs = vec![json!({ "name": "web", "port": 80 })];
    if extra_backend {
        backend_refs.push(json!({ "name": "web2", "port": 80, "weight": 50 }));
    }
    from_json(json!({
        "metadata": { "name": "route", "namespace": "demo" },
        "spec": {
            "parentRefs": [{ "name": "gw" }],
            "hostnames": ["app.example.com"],
            "rules": [{
                "matches": [{ "path": { "type": "PathPrefix", "value": "/" } }],
                "backendRefs": backend_refs
            }]
        }
    }))
}

#[test]
fn http_route_maps_to_ir() {
    let inputs = Inputs {
        gateway_classes: vec![gateway_class("sozu.io/gateway-controller")],
        gateways: vec![http_gateway()],
        http_routes: vec![route_to_web(false)],
        services: vec![web_service()],
        endpointslices: vec![web_slice()],
        ..Default::default()
    };
    let out = build(&BuildConfig::default(), &inputs);

    assert_eq!(out.ir.clusters.len(), 1);
    assert_eq!(out.ir.backends.len(), 2, "two pod IPs");
    assert_eq!(out.ir.frontends.len(), 1, "one HTTP frontend");
    assert!(!out.ir.frontends[0].tls);
    assert!(out.gateway_classes[0].accepted);
    assert!(out.gateways[0].programmed);
    assert_eq!(out.routes.len(), 1);
    assert!(out.routes[0].parents[0].resolved_refs);
    assert!(out.routes[0].parents[0].problems.is_empty());

    insta::assert_json_snapshot!(out.ir);
}

#[test]
fn https_listener_loads_cert() {
    let inputs = Inputs {
        gateway_classes: vec![gateway_class("sozu.io/gateway-controller")],
        gateways: vec![https_gateway()],
        http_routes: vec![route_to_web(false)],
        services: vec![web_service()],
        endpointslices: vec![web_slice()],
        secrets: vec![tls_secret()],
        ..Default::default()
    };
    let out = build(&BuildConfig::default(), &inputs);

    assert_eq!(out.ir.certificates.len(), 1, "listener cert loaded");
    assert_eq!(out.ir.frontends.len(), 1);
    assert!(out.ir.frontends[0].tls, "HTTPS frontend");
    assert!(out.routes[0].parents[0].resolved_refs);
}

#[test]
fn other_controller_is_ignored() {
    let inputs = Inputs {
        gateway_classes: vec![gateway_class("other.io/controller")],
        gateways: vec![http_gateway()],
        http_routes: vec![route_to_web(false)],
        services: vec![web_service()],
        endpointslices: vec![web_slice()],
        ..Default::default()
    };
    let out = build(&BuildConfig::default(), &inputs);

    assert!(!out.gateway_classes[0].accepted, "not our controllerName");
    assert!(
        out.gateways.is_empty(),
        "gateway of a foreign class is skipped"
    );
    assert!(out.ir.clusters.is_empty());
    assert!(out.ir.frontends.is_empty());
    assert!(out.routes.is_empty());
}

#[test]
fn weighted_backends_are_unsupported() {
    let inputs = Inputs {
        gateway_classes: vec![gateway_class("sozu.io/gateway-controller")],
        gateways: vec![http_gateway()],
        http_routes: vec![route_to_web(true)], // two backendRefs
        services: vec![web_service()],
        endpointslices: vec![web_slice()],
        ..Default::default()
    };
    let out = build(&BuildConfig::default(), &inputs);

    assert!(
        out.ir.frontends.is_empty(),
        "rule rejected, no route created"
    );
    let parent = &out.routes[0].parents[0];
    assert!(!parent.resolved_refs);
    assert!(parent
        .problems
        .contains(&Problem::WeightedBackendsUnsupported));
}

#[test]
fn http_route_filters_map_to_ir() {
    let route: HttpRoute = from_json(json!({
        "metadata": { "name": "route", "namespace": "demo" },
        "spec": {
            "parentRefs": [{ "name": "gw" }],
            "hostnames": ["app.example.com"],
            "rules": [{
                "matches": [{ "path": { "type": "PathPrefix", "value": "/" } }],
                "filters": [
                    { "type": "RequestHeaderModifier", "requestHeaderModifier": {
                        "set": [{ "name": "X-Env", "value": "prod" }],
                        "remove": ["X-Debug"] } },
                    { "type": "ResponseHeaderModifier", "responseHeaderModifier": {
                        "add": [{ "name": "X-Served-By", "value": "sozu" }] } },
                    { "type": "URLRewrite", "urlRewrite": {
                        "hostname": "backend.svc",
                        "path": { "type": "ReplaceFullPath", "replaceFullPath": "/v2" } } }
                ],
                "backendRefs": [{ "name": "web", "port": 80 }]
            }]
        }
    }));
    let inputs = Inputs {
        gateway_classes: vec![gateway_class("sozu.io/gateway-controller")],
        gateways: vec![http_gateway()],
        http_routes: vec![route],
        services: vec![web_service()],
        endpointslices: vec![web_slice()],
        ..Default::default()
    };
    let out = build(&BuildConfig::default(), &inputs);

    assert_eq!(out.ir.frontends.len(), 1);
    let f = &out.ir.frontends[0].filters;
    assert_eq!(f.header_mods.len(), 3);
    assert!(f
        .header_mods
        .iter()
        .any(|m| matches!(m.on, ir::HeaderTarget::Request)
            && m.key == "X-Env"
            && m.value.as_deref() == Some("prod")));
    assert!(f
        .header_mods
        .iter()
        .any(|m| matches!(m.on, ir::HeaderTarget::Request)
            && m.key == "X-Debug"
            && m.value.is_none())); // remove
    assert!(f
        .header_mods
        .iter()
        .any(|m| matches!(m.on, ir::HeaderTarget::Response) && m.key == "X-Served-By"));
    // URLRewrite is reported unsupported (Sōzu's rewrite_host targets the backend
    // authority, incompatible with Gateway semantics) rather than mapped.
    assert!(f.rewrite.is_none());
    assert!(out.routes[0].parents[0]
        .problems
        .iter()
        .any(|p| matches!(p, Problem::FilterUnsupported { kind } if kind == "URLRewrite")));
    assert!(out.routes[0].parents[0].resolved_refs);
}

#[test]
fn redirect_filter_supported_and_unsupported_reported() {
    let route: HttpRoute = from_json(json!({
        "metadata": { "name": "route", "namespace": "demo" },
        "spec": {
            "parentRefs": [{ "name": "gw" }],
            "hostnames": ["app.example.com"],
            "rules": [{
                "filters": [
                    { "type": "RequestRedirect", "requestRedirect": { "scheme": "https", "statusCode": 301 } },
                    { "type": "RequestMirror", "requestMirror": { "backendRef": { "name": "mirror", "port": 80 } } }
                ]
            }]
        }
    }));
    let inputs = Inputs {
        gateway_classes: vec![gateway_class("sozu.io/gateway-controller")],
        gateways: vec![http_gateway()],
        http_routes: vec![route],
        services: vec![web_service()],
        endpointslices: vec![web_slice()],
        ..Default::default()
    };
    let out = build(&BuildConfig::default(), &inputs);

    // Redirect-only route: a frontend with no cluster (backendRef-less).
    assert_eq!(out.ir.frontends.len(), 1);
    assert!(out.ir.frontends[0].cluster_id.is_none());
    let redirect = out.ir.frontends[0]
        .filters
        .redirect
        .as_ref()
        .expect("redirect");
    assert!(matches!(redirect.scheme, Some(ir::Scheme::Https)));
    assert!(matches!(
        redirect.status,
        ir::RedirectStatus::MovedPermanently
    ));
    // RequestMirror is not supported by Sōzu -> reported.
    assert!(out.routes[0].parents[0]
        .problems
        .iter()
        .any(|p| matches!(p, Problem::FilterUnsupported { .. })));
}

#[test]
fn gateway_and_ingress_share_one_cluster() {
    let ingress: Ingress = from_json(json!({
        "metadata": { "name": "web", "namespace": "demo" },
        "spec": { "ingressClassName": "sozu", "rules": [{
            "host": "ing.example.com",
            "http": { "paths": [{ "path": "/", "pathType": "Prefix",
                "backend": { "service": { "name": "web", "port": { "number": 80 } } } }] }
        }]}
    }));
    let inputs = Inputs {
        ingresses: vec![ingress],
        gateway_classes: vec![gateway_class("sozu.io/gateway-controller")],
        gateways: vec![http_gateway()],
        http_routes: vec![route_to_web(false)],
        services: vec![web_service()],
        endpointslices: vec![web_slice()],
        ..Default::default()
    };
    let out = build(&BuildConfig::default(), &inputs);

    // Both APIs target demo/web:80 -> a single shared cluster + deduped backends.
    assert_eq!(out.ir.clusters.len(), 1, "shared cluster");
    assert_eq!(out.ir.backends.len(), 2, "deduped backends");
    // Two HTTP frontends: one per host (ingress + gateway route).
    assert_eq!(out.ir.frontends.len(), 2);
}
