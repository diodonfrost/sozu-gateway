//! Behavioural tests for the Gateway API -> IR mapping (Phase 2).

use std::collections::BTreeMap;
use std::sync::Arc;

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

/// Wrap plain objects in the `Arc`s `Inputs` borrows (the controller passes
/// its reflector-cache `Arc`s straight through).
fn arcs<T>(items: Vec<T>) -> Vec<Arc<T>> {
    items.into_iter().map(Arc::new).collect()
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
        gateway_classes: arcs(vec![gateway_class("sozu.io/gateway-controller")]),
        gateways: arcs(vec![http_gateway()]),
        http_routes: arcs(vec![route_to_web(false)]),
        services: arcs(vec![web_service()]),
        endpointslices: arcs(vec![web_slice()]),
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
        gateway_classes: arcs(vec![gateway_class("sozu.io/gateway-controller")]),
        gateways: arcs(vec![https_gateway()]),
        http_routes: arcs(vec![route_to_web(false)]),
        services: arcs(vec![web_service()]),
        endpointslices: arcs(vec![web_slice()]),
        secrets: arcs(vec![tls_secret()]),
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
        gateway_classes: arcs(vec![gateway_class("other.io/controller")]),
        gateways: arcs(vec![http_gateway()]),
        http_routes: arcs(vec![route_to_web(false)]),
        services: arcs(vec![web_service()]),
        endpointslices: arcs(vec![web_slice()]),
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
        gateway_classes: arcs(vec![gateway_class("sozu.io/gateway-controller")]),
        gateways: arcs(vec![http_gateway()]),
        http_routes: arcs(vec![route_to_web(true)]), // two backendRefs
        services: arcs(vec![web_service()]),
        endpointslices: arcs(vec![web_slice()]),
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
fn zero_weight_single_backend_is_drained_not_served() {
    // weight: 0 on the (single) backendRef is the standard drain pattern: the
    // backend must receive NO traffic (the spec even calls for a 500 when all
    // weights are zero). Sōzu cannot weight or synthesize the 500, so the
    // rule is reported and skipped — never served at 100%.
    let route: HttpRoute = from_json(json!({
        "metadata": { "name": "route", "namespace": "demo" },
        "spec": {
            "parentRefs": [{ "name": "gw" }],
            "hostnames": ["app.example.com"],
            "rules": [{ "backendRefs": [{ "name": "web", "port": 80, "weight": 0 }] }]
        }
    }));
    let out = build(&BuildConfig::default(), &inputs_with(route));

    assert!(
        out.ir.frontends.is_empty(),
        "a drained backend gets nothing"
    );
    let p = &out.routes[0].parents[0];
    assert!(p.problems.contains(&Problem::ZeroWeightBackendUnsupported {
        service: "web".to_string(),
    }));
    // A skipped rule must show in the status, like every other skip path:
    // ResolvedRefs downgrades the same way the weighted-split rejection does.
    assert!(!p.resolved_refs, "the skipped rule must not read healthy");
    assert_eq!(p.resolved_refs_reason, "BackendNotFound");
}

#[test]
fn positive_weight_single_backend_still_routes() {
    // A single backendRef with any positive weight IS 100% — no problem.
    let route: HttpRoute = from_json(json!({
        "metadata": { "name": "route", "namespace": "demo" },
        "spec": {
            "parentRefs": [{ "name": "gw" }],
            "hostnames": ["app.example.com"],
            "rules": [{ "backendRefs": [{ "name": "web", "port": 80, "weight": 50 }] }]
        }
    }));
    let out = build(&BuildConfig::default(), &inputs_with(route));

    assert_eq!(out.ir.frontends.len(), 1);
    assert!(out.routes[0].parents[0].problems.is_empty());
}

#[test]
fn route_timeouts_are_reported_unsupported() {
    // Sōzu has no per-route timeout knob: the rule still routes (RequestMirror
    // precedent — drop the unsupported piece, never half-apply), but the user
    // must see that the timeout took no effect.
    let route: HttpRoute = from_json(json!({
        "metadata": { "name": "route", "namespace": "demo" },
        "spec": {
            "parentRefs": [{ "name": "gw" }],
            "hostnames": ["app.example.com"],
            "rules": [{
                "timeouts": { "request": "10s" },
                "backendRefs": [{ "name": "web", "port": 80 }]
            }]
        }
    }));
    let out = build(&BuildConfig::default(), &inputs_with(route));

    assert_eq!(out.ir.frontends.len(), 1, "the rule still routes");
    assert!(out.routes[0].parents[0]
        .problems
        .contains(&Problem::TimeoutsUnsupported));
}

#[test]
fn backend_ref_filters_are_reported_unsupported() {
    // Filters scoped to a backendRef have no Sōzu equivalent (filters wire
    // onto the frontend). They must be reported, not silently dropped — and
    // never half-applied onto the frontend.
    let route: HttpRoute = from_json(json!({
        "metadata": { "name": "route", "namespace": "demo" },
        "spec": {
            "parentRefs": [{ "name": "gw" }],
            "hostnames": ["app.example.com"],
            "rules": [{
                "backendRefs": [{ "name": "web", "port": 80, "filters": [
                    { "type": "RequestHeaderModifier", "requestHeaderModifier": {
                        "set": [{ "name": "X-Env", "value": "prod" }] } }
                ]}]
            }]
        }
    }));
    let out = build(&BuildConfig::default(), &inputs_with(route));

    assert_eq!(out.ir.frontends.len(), 1, "the backend still routes");
    assert!(
        out.ir.frontends[0].filters.header_mods.is_empty(),
        "the backendRef filter must not leak onto the frontend"
    );
    assert!(out.routes[0].parents[0].problems.iter().any(
        |p| matches!(p, Problem::FilterUnsupported { kind } if kind.contains("backendRef web"))
    ));
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
        gateway_classes: arcs(vec![gateway_class("sozu.io/gateway-controller")]),
        gateways: arcs(vec![http_gateway()]),
        http_routes: arcs(vec![route]),
        services: arcs(vec![web_service()]),
        endpointslices: arcs(vec![web_slice()]),
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
        gateway_classes: arcs(vec![gateway_class("sozu.io/gateway-controller")]),
        gateways: arcs(vec![http_gateway()]),
        http_routes: arcs(vec![route]),
        services: arcs(vec![web_service()]),
        endpointslices: arcs(vec![web_slice()]),
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
fn ingress_colliding_with_redirect_only_route_reports_the_ingress() {
    // A redirect-only HTTPRoute (cluster-less frontend) and an Ingress claim
    // the same host+path. The translator's dedup orders a cluster-less
    // frontend (None) before any cluster id, so the redirect wins; the losing
    // Ingress owner must see the collision.
    let route: HttpRoute = from_json(json!({
        "metadata": { "name": "redirect", "namespace": "demo" },
        "spec": {
            "parentRefs": [{ "name": "gw" }],
            "hostnames": ["app.example.com"],
            "rules": [{
                "filters": [
                    { "type": "RequestRedirect",
                      "requestRedirect": { "scheme": "https", "statusCode": 301 } }
                ]
            }]
        }
    }));
    let ingress: Ingress = from_json(json!({
        "metadata": { "name": "web", "namespace": "demo" },
        "spec": { "ingressClassName": "sozu", "rules": [{
            "host": "app.example.com",
            "http": { "paths": [{ "path": "/", "pathType": "Prefix",
                "backend": { "service": { "name": "web", "port": { "number": 80 } } } }] }
        }]}
    }));
    let inputs = Inputs {
        ingresses: arcs(vec![ingress]),
        gateway_classes: arcs(vec![gateway_class("sozu.io/gateway-controller")]),
        gateways: arcs(vec![http_gateway()]),
        http_routes: arcs(vec![route]),
        services: arcs(vec![web_service()]),
        endpointslices: arcs(vec![web_slice()]),
        ..Default::default()
    };
    let out = build(&BuildConfig::default(), &inputs);

    assert_eq!(out.ir.frontends.len(), 1, "one frontend per route key");
    assert!(
        out.ir.frontends[0].cluster_id.is_none(),
        "the redirect-only frontend wins"
    );
    assert_eq!(
        out.results[0].problems,
        vec![Problem::RouteCollision {
            hostname: "app.example.com".to_string(),
            path: "/".to_string(),
            winner: "<redirect>".to_string(),
        }],
        "the losing Ingress carries the collision"
    );
    assert!(
        out.routes[0].parents[0].problems.is_empty(),
        "the winning route stays clean"
    );
}

#[test]
fn losing_route_parent_is_not_accepted_with_route_collision_reason() {
    // Two HTTPRoutes claim app.example.com "/": the redirect-only route wins
    // (a cluster-less frontend orders before any cluster id) and the backend
    // route loses. The loser's parent must not read fully healthy: its
    // Accepted condition downgrades with the implementation-specific
    // RouteCollision reason, so kubectl shows the collision, not just a log.
    let redirect: HttpRoute = from_json(json!({
        "metadata": { "name": "redirect", "namespace": "demo" },
        "spec": {
            "parentRefs": [{ "name": "gw" }],
            "hostnames": ["app.example.com"],
            "rules": [{
                "filters": [
                    { "type": "RequestRedirect",
                      "requestRedirect": { "scheme": "https", "statusCode": 301 } }
                ]
            }]
        }
    }));
    let inputs = Inputs {
        gateway_classes: arcs(vec![gateway_class("sozu.io/gateway-controller")]),
        gateways: arcs(vec![http_gateway()]),
        http_routes: arcs(vec![route_to_web(false), redirect]),
        services: arcs(vec![web_service()]),
        endpointslices: arcs(vec![web_slice()]),
        ..Default::default()
    };
    let out = build(&BuildConfig::default(), &inputs);

    assert_eq!(out.ir.frontends.len(), 1, "one frontend per route key");
    assert!(out.ir.frontends[0].cluster_id.is_none(), "redirect wins");
    let loser = out
        .routes
        .iter()
        .find(|r| r.name == "route")
        .expect("losing route present");
    let p = &loser.parents[0];
    assert!(!p.accepted, "the losing parent must not read accepted");
    assert_eq!(p.accepted_reason, "RouteCollision");
    assert!(p.problems.contains(&Problem::RouteCollision {
        hostname: "app.example.com".to_string(),
        path: "/".to_string(),
        winner: "<redirect>".to_string(),
    }));
    let winner = out
        .routes
        .iter()
        .find(|r| r.name == "redirect")
        .expect("winning route present");
    assert!(winner.parents[0].accepted, "the winner stays clean");
    assert!(winner.parents[0].problems.is_empty());
}

#[test]
fn collision_lands_on_the_parent_ref_that_produced_the_frontend() {
    // One route, TWO parentRefs to the SAME Gateway distinguished only by
    // sectionName. Only the frontend produced via listener "b" collides; the
    // attribution must key on the full parentRef identity (sectionName), not
    // stop at the first (gateway_namespace, gateway_name) match.
    let gw: Gateway = from_json(json!({
        "metadata": { "name": "gw", "namespace": "demo" },
        "spec": { "gatewayClassName": "sozu", "listeners": [
            { "name": "a", "protocol": "HTTP", "port": 80, "hostname": "a.example.com" },
            { "name": "b", "protocol": "HTTP", "port": 80, "hostname": "b.example.com" }
        ]}
    }));
    let route: HttpRoute = from_json(json!({
        "metadata": { "name": "route", "namespace": "demo" },
        "spec": {
            "parentRefs": [
                { "name": "gw", "sectionName": "a" },
                { "name": "gw", "sectionName": "b" }
            ],
            "rules": [{ "backendRefs": [{ "name": "web", "port": 80 }] }]
        }
    }));
    // Redirect-only route pinned to b.example.com: wins that key only.
    let redirect: HttpRoute = from_json(json!({
        "metadata": { "name": "redirect", "namespace": "demo" },
        "spec": {
            "parentRefs": [{ "name": "gw", "sectionName": "b" }],
            "hostnames": ["b.example.com"],
            "rules": [{
                "filters": [
                    { "type": "RequestRedirect",
                      "requestRedirect": { "scheme": "https", "statusCode": 301 } }
                ]
            }]
        }
    }));
    let inputs = Inputs {
        gateway_classes: arcs(vec![gateway_class("sozu.io/gateway-controller")]),
        gateways: arcs(vec![gw]),
        http_routes: arcs(vec![route, redirect]),
        services: arcs(vec![web_service()]),
        endpointslices: arcs(vec![web_slice()]),
        ..Default::default()
    };
    let out = build(&BuildConfig::default(), &inputs);

    let r = out.routes.iter().find(|r| r.name == "route").unwrap();
    assert_eq!(r.parents.len(), 2);
    let parent_a = r
        .parents
        .iter()
        .find(|p| p.section_name.as_deref() == Some("a"))
        .unwrap();
    assert!(parent_a.accepted, "listener a did not collide");
    assert!(parent_a.problems.is_empty());
    let parent_b = r
        .parents
        .iter()
        .find(|p| p.section_name.as_deref() == Some("b"))
        .unwrap();
    assert!(
        !parent_b.accepted,
        "the collision is on listener b's parent"
    );
    assert_eq!(parent_b.accepted_reason, "RouteCollision");
    assert!(parent_b.problems.contains(&Problem::RouteCollision {
        hostname: "b.example.com".to_string(),
        path: "/".to_string(),
        winner: "<redirect>".to_string(),
    }));
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
        ingresses: arcs(vec![ingress]),
        gateway_classes: arcs(vec![gateway_class("sozu.io/gateway-controller")]),
        gateways: arcs(vec![http_gateway()]),
        http_routes: arcs(vec![route_to_web(false)]),
        services: arcs(vec![web_service()]),
        endpointslices: arcs(vec![web_slice()]),
        ..Default::default()
    };
    let out = build(&BuildConfig::default(), &inputs);

    // Both APIs target demo/web:80 -> a single shared cluster + deduped backends.
    assert_eq!(out.ir.clusters.len(), 1, "shared cluster");
    assert_eq!(out.ir.backends.len(), 2, "deduped backends");
    // Two HTTP frontends: one per host (ingress + gateway route).
    assert_eq!(out.ir.frontends.len(), 2);
}

#[test]
fn gateway_hostless_route_maps_to_catch_all() {
    // A route with no hostnames on a listener with no hostname is a catch-all:
    // it must produce a single `*` frontend (Sōzu DomainRule::Any), not be skipped.
    let route: HttpRoute = from_json(json!({
        "metadata": { "name": "route", "namespace": "demo" },
        "spec": {
            "parentRefs": [{ "name": "gw" }],
            "rules": [{
                "matches": [{ "path": { "type": "PathPrefix", "value": "/" } }],
                "backendRefs": [{ "name": "web", "port": 80 }]
            }]
        }
    }));
    let inputs = Inputs {
        gateway_classes: arcs(vec![gateway_class("sozu.io/gateway-controller")]),
        gateways: arcs(vec![http_gateway()]),
        http_routes: arcs(vec![route]),
        services: arcs(vec![web_service()]),
        endpointslices: arcs(vec![web_slice()]),
        ..Default::default()
    };
    let out = build(&BuildConfig::default(), &inputs);

    assert_eq!(out.ir.frontends.len(), 1);
    assert_eq!(out.ir.frontends[0].hostname, "*");
    assert!(!out.ir.frontends[0].tls);
    assert!(out.routes[0].parents[0].resolved_refs);
    assert!(out.routes[0].parents[0].problems.is_empty());
}

#[test]
fn route_hostname_not_matching_listener_is_silently_skipped() {
    // Listener constrained to a.example.com; route serves only b.example.com.
    // The route attaches elsewhere, so emit no frontend here AND no problem
    // (this is not a hostless rule).
    let gw: Gateway = from_json(json!({
        "metadata": { "name": "gw", "namespace": "demo" },
        "spec": { "gatewayClassName": "sozu", "listeners": [
            { "name": "http", "protocol": "HTTP", "port": 80, "hostname": "a.example.com" }
        ]}
    }));
    let route: HttpRoute = from_json(json!({
        "metadata": { "name": "route", "namespace": "demo" },
        "spec": {
            "parentRefs": [{ "name": "gw" }],
            "hostnames": ["b.example.com"],
            "rules": [{
                "matches": [{ "path": { "type": "PathPrefix", "value": "/" } }],
                "backendRefs": [{ "name": "web", "port": 80 }]
            }]
        }
    }));
    let inputs = Inputs {
        gateway_classes: arcs(vec![gateway_class("sozu.io/gateway-controller")]),
        gateways: arcs(vec![gw]),
        http_routes: arcs(vec![route]),
        services: arcs(vec![web_service()]),
        endpointslices: arcs(vec![web_slice()]),
        ..Default::default()
    };
    let out = build(&BuildConfig::default(), &inputs);

    assert!(out.ir.frontends.is_empty());
    assert!(out.routes[0].parents[0].problems.is_empty());
}

#[test]
fn wildcard_route_on_specific_listener_narrows_to_the_listener_hostname() {
    // Listener pinned to test.example.com; route hostname *.example.com. The
    // Gateway API intersects the two: only test.example.com may be served, so
    // the frontend must carry the listener's (more specific) hostname — a
    // *.example.com frontend would also route other.example.com, which this
    // listener never admits.
    let gw: Gateway = from_json(json!({
        "metadata": { "name": "gw", "namespace": "demo" },
        "spec": { "gatewayClassName": "sozu", "listeners": [
            { "name": "http", "protocol": "HTTP", "port": 80, "hostname": "test.example.com" }
        ]}
    }));
    let route: HttpRoute = from_json(json!({
        "metadata": { "name": "route", "namespace": "demo" },
        "spec": {
            "parentRefs": [{ "name": "gw" }],
            "hostnames": ["*.example.com"],
            "rules": [{ "backendRefs": [{ "name": "web", "port": 80 }] }]
        }
    }));
    let inputs = Inputs {
        gateway_classes: arcs(vec![gateway_class("sozu.io/gateway-controller")]),
        gateways: arcs(vec![gw]),
        http_routes: arcs(vec![route]),
        services: arcs(vec![web_service()]),
        endpointslices: arcs(vec![web_slice()]),
        ..Default::default()
    };
    let out = build(&BuildConfig::default(), &inputs);

    assert_eq!(out.ir.frontends.len(), 1);
    assert_eq!(out.ir.frontends[0].hostname, "test.example.com");
    assert!(out.routes[0].parents[0].accepted);
}

#[test]
fn specific_route_on_wildcard_listener_uses_the_route_hostname() {
    // Listener *.example.com; route pinned to test.example.com: the route's
    // (more specific) hostname is the intersection and must be programmed.
    let gw: Gateway = from_json(json!({
        "metadata": { "name": "gw", "namespace": "demo" },
        "spec": { "gatewayClassName": "sozu", "listeners": [
            { "name": "http", "protocol": "HTTP", "port": 80, "hostname": "*.example.com" }
        ]}
    }));
    let route: HttpRoute = from_json(json!({
        "metadata": { "name": "route", "namespace": "demo" },
        "spec": {
            "parentRefs": [{ "name": "gw" }],
            "hostnames": ["test.example.com"],
            "rules": [{ "backendRefs": [{ "name": "web", "port": 80 }] }]
        }
    }));
    let inputs = Inputs {
        gateway_classes: arcs(vec![gateway_class("sozu.io/gateway-controller")]),
        gateways: arcs(vec![gw]),
        http_routes: arcs(vec![route]),
        services: arcs(vec![web_service()]),
        endpointslices: arcs(vec![web_slice()]),
        ..Default::default()
    };
    let out = build(&BuildConfig::default(), &inputs);

    assert_eq!(out.ir.frontends.len(), 1);
    assert_eq!(out.ir.frontends[0].hostname, "test.example.com");
}

#[test]
fn equal_route_and_listener_hostname_is_programmed_unchanged() {
    // https_gateway() pins app.example.com; the route serves the same name.
    let inputs = Inputs {
        gateway_classes: arcs(vec![gateway_class("sozu.io/gateway-controller")]),
        gateways: arcs(vec![https_gateway()]),
        http_routes: arcs(vec![route_to_web(false)]),
        services: arcs(vec![web_service()]),
        endpointslices: arcs(vec![web_slice()]),
        secrets: arcs(vec![tls_secret()]),
        ..Default::default()
    };
    let out = build(&BuildConfig::default(), &inputs);
    assert_eq!(out.ir.frontends.len(), 1);
    assert_eq!(out.ir.frontends[0].hostname, "app.example.com");
}

fn inputs_with(route: HttpRoute) -> Inputs {
    Inputs {
        gateway_classes: arcs(vec![gateway_class("sozu.io/gateway-controller")]),
        gateways: arcs(vec![http_gateway()]),
        http_routes: arcs(vec![route]),
        services: arcs(vec![web_service()]),
        endpointslices: arcs(vec![web_slice()]),
        ..Default::default()
    }
}

#[test]
fn referenced_services_cover_httproute_backends_resolved_or_not() {
    // Two rules: one resolves to `web`, one targets a Service that does not
    // exist. Both must land in `referenced_services` — the EndpointSlice ping
    // filter feeds on it, and a slice appearing later for the still-missing
    // backend has to wake the reconcile loop.
    let route: HttpRoute = from_json(json!({
        "metadata": { "name": "route", "namespace": "demo" },
        "spec": {
            "parentRefs": [{ "name": "gw" }],
            "hostnames": ["app.example.com"],
            "rules": [
                { "matches": [{ "path": { "type": "PathPrefix", "value": "/" } }],
                  "backendRefs": [{ "name": "web", "port": 80 }] },
                { "matches": [{ "path": { "type": "PathPrefix", "value": "/missing" } }],
                  "backendRefs": [{ "name": "missing", "port": 80 }] }
            ]
        }
    }));
    let out = build(&BuildConfig::default(), &inputs_with(route));

    let referenced: Vec<&str> = out.referenced_services.iter().map(|s| s.as_str()).collect();
    assert_eq!(referenced, vec!["demo/missing", "demo/web"]);
    // Sanity: the second backend really did fail to resolve.
    assert!(out.routes[0].parents[0]
        .problems
        .contains(&Problem::ServiceNotFound {
            service: "missing".to_string()
        }));
}

#[test]
fn parentref_section_name_not_matching_listener_is_not_accepted() {
    // sectionName matches no listener -> Accepted=False / NoMatchingParent.
    let route: HttpRoute = from_json(json!({
        "metadata": { "name": "route", "namespace": "demo" },
        "spec": {
            "parentRefs": [{ "name": "gw", "sectionName": "does-not-exist" }],
            "rules": [{ "backendRefs": [{ "name": "web", "port": 80 }] }]
        }
    }));
    let out = build(&BuildConfig::default(), &inputs_with(route));
    let p = &out.routes[0].parents[0];
    assert!(!p.accepted);
    assert_eq!(p.accepted_reason, "NoMatchingParent");
    assert!(out.ir.frontends.is_empty());
}

#[test]
fn non_service_backend_ref_is_invalid_kind() {
    let route: HttpRoute = from_json(json!({
        "metadata": { "name": "route", "namespace": "demo" },
        "spec": {
            "parentRefs": [{ "name": "gw" }],
            "hostnames": ["app.example.com"],
            "rules": [{ "backendRefs": [{ "group": "x.io", "kind": "Foo", "name": "web", "port": 80 }] }]
        }
    }));
    let out = build(&BuildConfig::default(), &inputs_with(route));
    let p = &out.routes[0].parents[0];
    assert!(p.accepted);
    assert!(!p.resolved_refs);
    assert_eq!(p.resolved_refs_reason, "InvalidKind");
}

#[test]
fn nonexistent_backend_ref_is_backend_not_found() {
    let route: HttpRoute = from_json(json!({
        "metadata": { "name": "route", "namespace": "demo" },
        "spec": {
            "parentRefs": [{ "name": "gw" }],
            "hostnames": ["app.example.com"],
            "rules": [{ "backendRefs": [{ "name": "ghost", "port": 80 }] }]
        }
    }));
    let out = build(&BuildConfig::default(), &inputs_with(route));
    let p = &out.routes[0].parents[0];
    assert!(!p.resolved_refs);
    assert_eq!(p.resolved_refs_reason, "BackendNotFound");
}

#[test]
fn cross_namespace_backend_without_grant_is_ref_not_permitted() {
    let route: HttpRoute = from_json(json!({
        "metadata": { "name": "route", "namespace": "demo" },
        "spec": {
            "parentRefs": [{ "name": "gw" }],
            "hostnames": ["app.example.com"],
            "rules": [{ "backendRefs": [{ "namespace": "other", "name": "web", "port": 80 }] }]
        }
    }));
    let out = build(&BuildConfig::default(), &inputs_with(route));
    let p = &out.routes[0].parents[0];
    assert!(!p.resolved_refs);
    assert_eq!(p.resolved_refs_reason, "RefNotPermitted");
}

#[test]
fn cross_namespace_route_to_same_listener_is_not_allowed() {
    // http_gateway() (ns "demo") has no allowedRoutes -> default `Same`. A route in
    // another namespace must NOT bind: Accepted=False / NotAllowedByListeners.
    let route: HttpRoute = from_json(json!({
        "metadata": { "name": "route", "namespace": "other" },
        "spec": {
            "parentRefs": [{ "name": "gw", "namespace": "demo" }],
            "rules": [{ "backendRefs": [{ "name": "web", "port": 80 }] }]
        }
    }));
    let out = build(&BuildConfig::default(), &inputs_with(route));
    let p = &out.routes[0].parents[0];
    assert!(!p.accepted);
    assert_eq!(p.accepted_reason, "NotAllowedByListeners");
    assert!(out.ir.frontends.is_empty());
}

#[test]
fn cross_namespace_route_to_all_listener_is_accepted() {
    // A listener with `allowedRoutes.namespaces.from: All` admits routes from any
    // namespace (the backend ref is unrelated to this assertion).
    let gw: Gateway = from_json(json!({
        "metadata": { "name": "gw", "namespace": "demo" },
        "spec": { "gatewayClassName": "sozu", "listeners": [
            { "name": "http", "protocol": "HTTP", "port": 80,
              "allowedRoutes": { "namespaces": { "from": "All" } } }
        ]}
    }));
    let route: HttpRoute = from_json(json!({
        "metadata": { "name": "route", "namespace": "other" },
        "spec": {
            "parentRefs": [{ "name": "gw", "namespace": "demo" }],
            "rules": [{ "backendRefs": [{ "name": "web", "port": 80 }] }]
        }
    }));
    let inputs = Inputs {
        gateway_classes: arcs(vec![gateway_class("sozu.io/gateway-controller")]),
        gateways: arcs(vec![gw]),
        http_routes: arcs(vec![route]),
        services: arcs(vec![web_service()]),
        endpointslices: arcs(vec![web_slice()]),
        ..Default::default()
    };
    let out = build(&BuildConfig::default(), &inputs);
    let p = &out.routes[0].parents[0];
    assert!(p.accepted);
    assert_eq!(p.accepted_reason, "Accepted");
}

#[test]
fn selector_listener_fails_closed_and_is_reported() {
    // `allowedRoutes.namespaces.from: Selector` cannot be evaluated (no
    // Namespace label index). It must fail CLOSED: no route is admitted from
    // ANY namespace (a selector replaces Same, it does not extend it), the
    // listener must not read cleanly Programmed, and the gap is reported —
    // never silently widened to All.
    let gw: Gateway = from_json(json!({
        "metadata": { "name": "gw", "namespace": "demo" },
        "spec": { "gatewayClassName": "sozu", "listeners": [
            { "name": "http", "protocol": "HTTP", "port": 80,
              "allowedRoutes": { "namespaces": { "from": "Selector",
                  "selector": { "matchLabels": { "team": "web" } } } } }
        ]}
    }));
    let cross_ns_route: HttpRoute = from_json(json!({
        "metadata": { "name": "cross", "namespace": "other" },
        "spec": {
            "parentRefs": [{ "name": "gw", "namespace": "demo" }],
            "rules": [{ "backendRefs": [{ "name": "web", "port": 80 }] }]
        }
    }));
    let same_ns_route: HttpRoute = from_json(json!({
        "metadata": { "name": "same", "namespace": "demo" },
        "spec": {
            "parentRefs": [{ "name": "gw" }],
            "rules": [{ "backendRefs": [{ "name": "web", "port": 80 }] }]
        }
    }));
    let inputs = Inputs {
        gateway_classes: arcs(vec![gateway_class("sozu.io/gateway-controller")]),
        gateways: arcs(vec![gw]),
        http_routes: arcs(vec![cross_ns_route, same_ns_route]),
        services: arcs(vec![web_service()]),
        endpointslices: arcs(vec![web_slice()]),
        ..Default::default()
    };
    let out = build(&BuildConfig::default(), &inputs);

    assert!(out.ir.frontends.is_empty(), "no route admitted");
    for route in &out.routes {
        let p = &route.parents[0];
        assert!(!p.accepted, "{} must not be admitted", route.name);
        assert_eq!(p.accepted_reason, "NotAllowedByListeners");
    }
    assert!(out.gateways[0]
        .problems
        .contains(&Problem::NamespaceSelectorUnsupported {
            listener: "http".to_string(),
        }));
    let l = &out.gateways[0].listeners[0];
    assert!(!l.programmed, "listener must not read cleanly Programmed");
    assert_eq!(l.programmed_reason, "Invalid");
}

#[test]
fn selector_https_listener_loads_no_certificates() {
    // Fail closed means ALL the way closed: an HTTPS listener with perfectly
    // valid certificateRefs but `from: Selector` admits no routes, so its
    // certificates must not be loaded into Sōzu either — exactly like the
    // port-mismatch path, which skips cert loading entirely.
    let gw: Gateway = from_json(json!({
        "metadata": { "name": "gw", "namespace": "demo" },
        "spec": { "gatewayClassName": "sozu", "listeners": [{
            "name": "https", "protocol": "HTTPS", "port": 443,
            "hostname": "app.example.com",
            "tls": { "mode": "Terminate", "certificateRefs": [{ "name": "app-tls" }] },
            "allowedRoutes": { "namespaces": { "from": "Selector",
                "selector": { "matchLabels": { "team": "web" } } } }
        }]}
    }));
    let inputs = Inputs {
        gateway_classes: arcs(vec![gateway_class("sozu.io/gateway-controller")]),
        gateways: arcs(vec![gw]),
        http_routes: arcs(vec![route_to_web(false)]),
        services: arcs(vec![web_service()]),
        endpointslices: arcs(vec![web_slice()]),
        secrets: arcs(vec![tls_secret()]),
        ..Default::default()
    };
    let out = build(&BuildConfig::default(), &inputs);

    assert!(
        out.ir.certificates.is_empty(),
        "a Selector listener must load no certificates"
    );
    assert!(out.ir.frontends.is_empty(), "and admit no routes");
    let l = &out.gateways[0].listeners[0];
    assert!(!l.programmed);
    assert_eq!(l.programmed_reason, "Invalid");
    assert!(out.gateways[0]
        .problems
        .contains(&Problem::NamespaceSelectorUnsupported {
            listener: "https".to_string(),
        }));
}

#[test]
fn listener_port_mismatch_is_reported_and_not_programmed() {
    // The advertised gateway ports default to 80/443: a listener declaring
    // port 8080 is not served on any client-visible port. Its routes must
    // NOT silently land on :80 — fail closed and report the mismatch — and
    // a route whose ONLY matching listener is port-mismatched must not read
    // healthy or count toward attachedRoutes.
    let gw: Gateway = from_json(json!({
        "metadata": { "name": "gw", "namespace": "demo" },
        "spec": { "gatewayClassName": "sozu", "listeners": [
            { "name": "http-alt", "protocol": "HTTP", "port": 8080 }
        ]}
    }));
    let route: HttpRoute = from_json(json!({
        "metadata": { "name": "route", "namespace": "demo" },
        "spec": {
            "parentRefs": [{ "name": "gw" }],
            "hostnames": ["app.example.com"],
            "rules": [{ "backendRefs": [{ "name": "web", "port": 80 }] }]
        }
    }));
    let inputs = Inputs {
        gateway_classes: arcs(vec![gateway_class("sozu.io/gateway-controller")]),
        gateways: arcs(vec![gw]),
        http_routes: arcs(vec![route]),
        services: arcs(vec![web_service()]),
        endpointslices: arcs(vec![web_slice()]),
        ..Default::default()
    };
    let out = build(&BuildConfig::default(), &inputs);

    assert!(out.ir.frontends.is_empty(), "no traffic on the wrong port");
    assert!(out.gateways[0]
        .problems
        .contains(&Problem::ListenerPortMismatch {
            listener: "http-alt".to_string(),
            declared: 8080,
            expected: 80,
        }));
    let l = &out.gateways[0].listeners[0];
    assert!(!l.accepted);
    assert_eq!(l.accepted_reason, "PortUnavailable");
    assert!(!l.programmed);
    assert_eq!(l.programmed_reason, "Invalid");
    assert_eq!(
        l.attached_routes, 0,
        "a mismatched listener carries no routes"
    );
    let p = &out.routes[0].parents[0];
    assert!(!p.accepted, "the route must not read healthy");
    assert_eq!(p.accepted_reason, "NotAllowedByListeners");
}

#[test]
fn listener_on_the_advertised_port_is_programmed() {
    // The check compares against the *configured* advertised port, not a
    // hardcoded 80: with gateway_http_port overridden to 8080, a listener
    // declaring 8080 programs fine.
    let gw: Gateway = from_json(json!({
        "metadata": { "name": "gw", "namespace": "demo" },
        "spec": { "gatewayClassName": "sozu", "listeners": [
            { "name": "http", "protocol": "HTTP", "port": 8080 }
        ]}
    }));
    let route: HttpRoute = from_json(json!({
        "metadata": { "name": "route", "namespace": "demo" },
        "spec": {
            "parentRefs": [{ "name": "gw" }],
            "hostnames": ["app.example.com"],
            "rules": [{ "backendRefs": [{ "name": "web", "port": 80 }] }]
        }
    }));
    let inputs = Inputs {
        gateway_classes: arcs(vec![gateway_class("sozu.io/gateway-controller")]),
        gateways: arcs(vec![gw]),
        http_routes: arcs(vec![route]),
        services: arcs(vec![web_service()]),
        endpointslices: arcs(vec![web_slice()]),
        ..Default::default()
    };
    let cfg = BuildConfig {
        gateway_http_port: 8080,
        ..Default::default()
    };
    let out = build(&cfg, &inputs);

    assert_eq!(out.ir.frontends.len(), 1);
    let l = &out.gateways[0].listeners[0];
    assert!(l.accepted && l.programmed);
    assert!(out.gateways[0].problems.is_empty());
}

#[test]
fn standard_gateway_ports_are_accepted_on_unprivileged_binds() {
    // The shipped chart binds the pod on 8080/8443 while the LoadBalancer
    // Service exposes 80/443. `listener.port` is the client-visible port, so
    // a standard Gateway declaring 80/443 MUST be accepted under that config
    // — comparing against the bind ports would reject every default install.
    let gw: Gateway = from_json(json!({
        "metadata": { "name": "gw", "namespace": "demo" },
        "spec": { "gatewayClassName": "sozu", "listeners": [
            { "name": "http", "protocol": "HTTP", "port": 80 },
            { "name": "https", "protocol": "HTTPS", "port": 443,
              "hostname": "app.example.com",
              "tls": { "mode": "Terminate", "certificateRefs": [{ "name": "app-tls" }] } }
        ]}
    }));
    let inputs = Inputs {
        gateway_classes: arcs(vec![gateway_class("sozu.io/gateway-controller")]),
        gateways: arcs(vec![gw]),
        http_routes: arcs(vec![route_to_web(false)]),
        services: arcs(vec![web_service()]),
        endpointslices: arcs(vec![web_slice()]),
        secrets: arcs(vec![tls_secret()]),
        ..Default::default()
    };
    let cfg = BuildConfig {
        // The chart's pod-level binds; advertised ports keep their 80/443
        // defaults, as the chart wires them from the Service values.
        http_listener: "0.0.0.0:8080".parse().expect("addr"),
        https_listener: "0.0.0.0:8443".parse().expect("addr"),
        ..Default::default()
    };
    let out = build(&cfg, &inputs);

    assert!(out.gateways[0].problems.is_empty(), "no port mismatch");
    for l in &out.gateways[0].listeners {
        assert!(l.accepted && l.programmed, "listener {} healthy", l.name);
    }
    assert_eq!(out.ir.frontends.len(), 2, "HTTP + HTTPS frontends emitted");
    assert_eq!(out.ir.certificates.len(), 1, "listener cert loaded");
    assert!(out.routes[0].parents[0].accepted);
}

#[test]
fn gateway_listener_status_counts_attached_routes() {
    let gw: Gateway = from_json(json!({
        "metadata": { "name": "gw", "namespace": "demo" },
        "spec": { "gatewayClassName": "sozu", "listeners": [
            { "name": "http", "protocol": "HTTP", "port": 80 },
            { "name": "http-unattached", "protocol": "HTTP", "port": 80 }
        ]}
    }));
    let route: HttpRoute = from_json(json!({
        "metadata": { "name": "route", "namespace": "demo" },
        "spec": {
            "parentRefs": [{ "name": "gw", "sectionName": "http" }],
            "hostnames": ["app.example.com"],
            "rules": [{ "backendRefs": [{ "name": "web", "port": 80 }] }]
        }
    }));
    let inputs = Inputs {
        gateway_classes: arcs(vec![gateway_class("sozu.io/gateway-controller")]),
        gateways: arcs(vec![gw]),
        http_routes: arcs(vec![route]),
        services: arcs(vec![web_service()]),
        endpointslices: arcs(vec![web_slice()]),
        ..Default::default()
    };
    let out = build(&BuildConfig::default(), &inputs);
    let g = &out.gateways[0];
    assert_eq!(g.listeners.len(), 2, "status for every declared listener");
    let http = g.listeners.iter().find(|l| l.name == "http").unwrap();
    assert_eq!(http.attached_routes, 1);
    assert_eq!(http.supported_kinds, vec!["HTTPRoute".to_string()]);
    assert!(http.accepted && http.programmed && http.resolved_refs);
    let unattached = g
        .listeners
        .iter()
        .find(|l| l.name == "http-unattached")
        .unwrap();
    assert_eq!(unattached.attached_routes, 0);
}

#[test]
fn gateway_listener_invalid_route_kind() {
    // allowedRoutes.kinds requests a kind we don't serve -> supportedKinds empty,
    // ResolvedRefs=False / InvalidRouteKinds.
    let gw: Gateway = from_json(json!({
        "metadata": { "name": "gw", "namespace": "demo" },
        "spec": { "gatewayClassName": "sozu", "listeners": [
            { "name": "http", "protocol": "HTTP", "port": 80,
              "allowedRoutes": { "kinds": [{ "kind": "TCPRoute" }] } }
        ]}
    }));
    let inputs = Inputs {
        gateway_classes: arcs(vec![gateway_class("sozu.io/gateway-controller")]),
        gateways: arcs(vec![gw]),
        ..Default::default()
    };
    let out = build(&BuildConfig::default(), &inputs);
    let l = &out.gateways[0].listeners[0];
    assert!(l.supported_kinds.is_empty());
    assert!(!l.resolved_refs);
    assert_eq!(l.resolved_refs_reason, "InvalidRouteKinds");
    assert_eq!(l.attached_routes, 0);
}

#[test]
fn cross_namespace_cert_without_grant_is_ref_not_permitted() {
    // HTTPS listener whose certificateRef is in another namespace, with no
    // ReferenceGrant -> listener ResolvedRefs=False / RefNotPermitted, unprogrammed.
    let gw: Gateway = from_json(json!({
        "metadata": { "name": "gw", "namespace": "demo" },
        "spec": { "gatewayClassName": "sozu", "listeners": [{
            "name": "https", "protocol": "HTTPS", "port": 443,
            "hostname": "app.example.com",
            "tls": { "mode": "Terminate",
                     "certificateRefs": [{ "name": "app-tls", "namespace": "certs" }] }
        }]}
    }));
    let inputs = Inputs {
        gateway_classes: arcs(vec![gateway_class("sozu.io/gateway-controller")]),
        gateways: arcs(vec![gw]),
        ..Default::default()
    };
    let out = build(&BuildConfig::default(), &inputs);
    let l = &out.gateways[0].listeners[0];
    assert!(!l.programmed);
    assert!(!l.resolved_refs);
    assert_eq!(l.resolved_refs_reason, "RefNotPermitted");
}

#[test]
fn cert_grant_with_wrong_from_group_is_not_permitted() {
    // A ReferenceGrant in the right namespace but with a non-matching `from.group`
    // must NOT permit the ref (group is part of the match).
    let gw: Gateway = from_json(json!({
        "metadata": { "name": "gw", "namespace": "demo" },
        "spec": { "gatewayClassName": "sozu", "listeners": [{
            "name": "https", "protocol": "HTTPS", "port": 443, "hostname": "app.example.com",
            "tls": { "mode": "Terminate",
                     "certificateRefs": [{ "name": "app-tls", "namespace": "certs" }] }
        }]}
    }));
    let inputs = Inputs {
        gateway_classes: arcs(vec![gateway_class("sozu.io/gateway-controller")]),
        gateways: arcs(vec![gw]),
        reference_grants: arcs(vec![from_json(json!({
            "metadata": { "name": "g", "namespace": "certs" },
            "spec": {
                "from": [{ "group": "wrong.group", "kind": "Gateway", "namespace": "demo" }],
                "to": [{ "group": "", "kind": "Secret" }]
            }
        }))]),
        ..Default::default()
    };
    let out = build(&BuildConfig::default(), &inputs);
    let l = &out.gateways[0].listeners[0];
    assert!(!l.resolved_refs);
    assert_eq!(l.resolved_refs_reason, "RefNotPermitted");
}
