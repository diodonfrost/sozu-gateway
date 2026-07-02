//! Gateway API status reporting (Phase 2).
//!
//! Writes Accepted/Programmed (Gateway, GatewayClass) and Accepted/ResolvedRefs
//! (HTTPRoute, per parent) conditions back to the objects.
//!
//! **Loop-safe:** it reads the current status, reuses `lastTransitionTime` for
//! conditions whose (status, reason, message) are unchanged, and skips the PATCH
//! entirely when nothing changed — so the controller's own status writes never
//! re-trigger a reconcile. **Best-effort:** every failure is logged, never
//! propagated, so status reporting can never break routing.

use k8s_openapi::api::core::v1::Service;
use k8s_openapi::api::networking::v1::{Ingress, IngressLoadBalancerIngress};
use k8s_openapi::apimachinery::pkg::apis::meta::v1::{Condition, Time};
use kube::api::{Patch, PatchParams};
use kube::{Api, Client};
use serde_json::json;
use tracing::{debug, warn};

use sozu_gw_builder::{GatewayClassResult, GatewayResult, IngressResult, Problem, RouteResult};
use sozu_gw_gateway_api::gateway::{
    GatewayStatusAddresses, GatewayStatusListeners, GatewayStatusListenersSupportedKinds,
};
use sozu_gw_gateway_api::httproute::{HttpRouteStatusParents, HttpRouteStatusParentsParentRef};
use sozu_gw_gateway_api::{Gateway, GatewayClass, HttpRoute};

const GW_GROUP: &str = "gateway.networking.k8s.io";

/// One desired condition before timestamping.
struct Desired {
    type_: &'static str,
    status: bool,
    reason: &'static str,
    message: String,
}

/// Compose a condition message from problem details, so `kubectl describe`
/// shows *which* Secret/Service/port is wrong instead of a generic sentence
/// (the detail otherwise only reaches controller logs). Sorted and deduped —
/// the message participates in `lastTransitionTime` reuse, so it must be
/// deterministic across reconciles — and capped so a pathological object
/// cannot bloat its own status.
fn problems_message(problems: &[&Problem], fallback: &str) -> String {
    if problems.is_empty() {
        return fallback.to_string();
    }
    let mut lines: Vec<String> = problems.iter().map(|p| p.to_string()).collect();
    lines.sort();
    lines.dedup();
    const MAX_SHOWN: usize = 5;
    let extra = lines.len().saturating_sub(MAX_SHOWN);
    let mut msg = lines[..lines.len().min(MAX_SHOWN)].join("; ");
    if extra > 0 {
        msg.push_str(&format!(" (+{extra} more)"));
    }
    msg
}

pub async fn write_status(
    client: &Client,
    controller_name: &str,
    gateway_classes: &[GatewayClassResult],
    gateways: &[GatewayResult],
    routes: &[RouteResult],
    gateway_addresses: &[GatewayStatusAddresses],
) {
    for gc in gateway_classes.iter().filter(|gc| gc.accepted) {
        if let Err(e) = write_gatewayclass(client, gc).await {
            warn!(name = %gc.name, error = %e, "failed to write GatewayClass status");
        }
    }
    for gw in gateways {
        if let Err(e) = write_gateway(client, gw, gateway_addresses).await {
            warn!(namespace = %gw.namespace, name = %gw.name, error = %e, "failed to write Gateway status");
        }
    }
    for route in routes {
        if let Err(e) = write_route(client, controller_name, route).await {
            warn!(namespace = %route.namespace, name = %route.name, error = %e, "failed to write HTTPRoute status");
        }
    }
}

fn now() -> Time {
    Time(k8s_openapi::jiff::Timestamp::now())
}

/// Build conditions, reusing the previous `lastTransitionTime` when a condition's
/// observable fields are unchanged (so repeated writes are byte-identical).
///
/// `observed_generation` is set to the object's `metadata.generation`: the Gateway
/// API requires every condition to carry it, and conformance checks that it tracks
/// the latest generation. `lastTransitionTime` still only moves when `status`
/// flips — a generation bump alone updates `observedGeneration` without resetting
/// the transition time.
fn build_conditions(
    desired: &[Desired],
    current: Option<&[Condition]>,
    generation: Option<i64>,
) -> Vec<Condition> {
    desired
        .iter()
        .map(|d| {
            let status = if d.status { "True" } else { "False" }.to_string();
            let previous = current.and_then(|cs| cs.iter().find(|c| c.type_ == d.type_));
            let last_transition_time = match previous {
                Some(p) if p.status == status && p.reason == d.reason && p.message == d.message => {
                    p.last_transition_time.clone()
                }
                _ => now(),
            };
            Condition {
                type_: d.type_.to_string(),
                status,
                reason: d.reason.to_string(),
                message: d.message.clone(),
                last_transition_time,
                observed_generation: generation,
            }
        })
        .collect()
}

fn conditions_equal(a: &[Condition], b: &[Condition]) -> bool {
    a.len() == b.len() && a.iter().zip(b).all(|(x, y)| x == y)
}

async fn write_gatewayclass(client: &Client, gc: &GatewayClassResult) -> Result<(), kube::Error> {
    let api: Api<GatewayClass> = Api::all(client.clone());
    let current = api.get(&gc.name).await?;
    let cur = current
        .status
        .as_ref()
        .and_then(|s| s.conditions.as_deref());
    let desired = build_conditions(
        &[Desired {
            type_: "Accepted",
            status: true,
            reason: "Accepted",
            message: "Accepted by sozu-gateway".to_string(),
        }],
        cur,
        current.metadata.generation,
    );
    if cur.is_some_and(|c| conditions_equal(&desired, c)) {
        return Ok(());
    }
    let patch = json!({ "status": { "conditions": desired } });
    api.patch_status(&gc.name, &PatchParams::default(), &Patch::Merge(&patch))
        .await?;
    debug!(name = %gc.name, "GatewayClass status updated");
    Ok(())
}

/// Build `Gateway.status.listeners[]`, reusing each listener condition's previous
/// `lastTransitionTime` (matched by listener name) so repeated writes are stable.
fn build_listeners_status(
    gw: &GatewayResult,
    current: &Gateway,
    generation: Option<i64>,
) -> Vec<GatewayStatusListeners> {
    let cur_listeners = current
        .status
        .as_ref()
        .and_then(|s| s.listeners.as_deref())
        .unwrap_or_default();
    gw.listeners
        .iter()
        .map(|l| {
            let prev = cur_listeners
                .iter()
                .find(|cl| cl.name == l.name)
                .map(|cl| cl.conditions.as_slice());
            // Problems that name this listener carry the user-facing detail
            // for its False conditions.
            let listener_problems: Vec<&Problem> = gw
                .problems
                .iter()
                .filter(|p| p.listener() == Some(l.name.as_str()))
                .collect();
            let conditions = build_conditions(
                &[
                    Desired {
                        type_: "Accepted",
                        status: l.accepted,
                        reason: l.accepted_reason,
                        message: if l.accepted {
                            "Listener accepted by sozu-gateway".to_string()
                        } else {
                            problems_message(
                                &listener_problems,
                                "Listener cannot be accepted as declared",
                            )
                        },
                    },
                    Desired {
                        type_: "Programmed",
                        status: l.programmed,
                        reason: l.programmed_reason,
                        message: if l.programmed {
                            "Listener programmed into Sōzu".to_string()
                        } else {
                            problems_message(
                                &listener_problems,
                                "Listener could not be programmed into Sōzu",
                            )
                        },
                    },
                    Desired {
                        type_: "ResolvedRefs",
                        status: l.resolved_refs,
                        reason: l.resolved_refs_reason,
                        message: if l.resolved_refs {
                            "Listener references resolved".to_string()
                        } else {
                            problems_message(
                                &listener_problems,
                                "Listener references could not be resolved",
                            )
                        },
                    },
                ],
                prev,
                generation,
            );
            GatewayStatusListeners {
                name: l.name.clone(),
                supported_kinds: l
                    .supported_kinds
                    .iter()
                    .map(|k| GatewayStatusListenersSupportedKinds {
                        group: Some(GW_GROUP.to_string()),
                        kind: k.clone(),
                    })
                    .collect(),
                attached_routes: l.attached_routes,
                conditions,
            }
        })
        .collect()
}

async fn write_gateway(
    client: &Client,
    gw: &GatewayResult,
    addresses: &[GatewayStatusAddresses],
) -> Result<(), kube::Error> {
    let api: Api<Gateway> = Api::namespaced(client.clone(), &gw.namespace);
    let current = api.get(&gw.name).await?;
    let cur = current
        .status
        .as_ref()
        .and_then(|s| s.conditions.as_deref());
    let all_problems: Vec<&Problem> = gw.problems.iter().collect();
    let desired = build_conditions(
        &[
            Desired {
                type_: "Accepted",
                status: gw.accepted,
                reason: if gw.accepted { "Accepted" } else { "Invalid" },
                message: if gw.accepted {
                    "Accepted by sozu-gateway".to_string()
                } else {
                    problems_message(&all_problems, "Gateway rejected")
                },
            },
            Desired {
                type_: "Programmed",
                status: gw.programmed,
                reason: if gw.programmed {
                    "Programmed"
                } else {
                    "Invalid"
                },
                message: if gw.programmed {
                    "Listeners programmed into Sōzu".to_string()
                } else {
                    problems_message(&all_problems, "No listeners could be programmed")
                },
            },
        ],
        cur,
        current.metadata.generation,
    );
    let listeners = build_listeners_status(gw, &current, current.metadata.generation);
    // Publish the LoadBalancer address into the Gateway's status (what
    // external-dns's gateway-httproute source reads). Skipped when there is no
    // address yet, so a pending LB never clears it.
    let cur_addresses = current
        .status
        .as_ref()
        .and_then(|s| s.addresses.as_deref())
        .unwrap_or_default();
    let cur_listeners = current
        .status
        .as_ref()
        .and_then(|s| s.listeners.as_deref())
        .unwrap_or_default();
    let addresses_unchanged = addresses.is_empty()
        || serde_json::to_value(cur_addresses).ok() == serde_json::to_value(addresses).ok();
    let listeners_unchanged =
        serde_json::to_value(cur_listeners).ok() == serde_json::to_value(&listeners).ok();
    let conditions_unchanged = cur.is_some_and(|c| conditions_equal(&desired, c));
    if conditions_unchanged && addresses_unchanged && listeners_unchanged {
        return Ok(());
    }
    let mut status = serde_json::Map::new();
    status.insert("conditions".to_string(), json!(desired));
    status.insert("listeners".to_string(), json!(listeners));
    if !addresses.is_empty() {
        status.insert("addresses".to_string(), json!(addresses));
    }
    let patch = json!({ "status": status });
    api.patch_status(&gw.name, &PatchParams::default(), &Patch::Merge(&patch))
        .await?;
    debug!(namespace = %gw.namespace, name = %gw.name, "Gateway status updated");
    Ok(())
}

/// Map the publish Service's load-balancer address(es) to Gateway status
/// addresses (`IPAddress` for an IP, `Hostname` otherwise).
pub(crate) fn gateway_addresses(svc: &Service) -> Vec<GatewayStatusAddresses> {
    lb_points(svc)
        .into_iter()
        .filter_map(|p| {
            if let Some(ip) = p.ip {
                Some(GatewayStatusAddresses {
                    r#type: Some("IPAddress".to_string()),
                    value: ip,
                })
            } else {
                p.hostname.map(|h| GatewayStatusAddresses {
                    r#type: Some("Hostname".to_string()),
                    value: h,
                })
            }
        })
        .collect()
}

async fn write_route(
    client: &Client,
    controller_name: &str,
    route: &RouteResult,
) -> Result<(), kube::Error> {
    let api: Api<HttpRoute> = Api::namespaced(client.clone(), &route.namespace);
    let current = api.get(&route.name).await?;
    let generation = current.metadata.generation;
    let current_parents: Vec<HttpRouteStatusParents> =
        current.status.map(|s| s.parents).unwrap_or_default();

    // Keep parent entries owned by other controllers untouched.
    let mut parents: Vec<HttpRouteStatusParents> = current_parents
        .iter()
        .filter(|p| p.controller_name != controller_name)
        .cloned()
        .collect();

    for parent in &route.parents {
        let existing = current_parents.iter().find(|p| {
            p.controller_name == controller_name
                && p.parent_ref.name == parent.gateway_name
                && p.parent_ref.namespace.as_deref() == Some(parent.gateway_namespace.as_str())
        });
        let parent_problems: Vec<&Problem> = parent.problems.iter().collect();
        let conditions = build_conditions(
            &[
                Desired {
                    type_: "Accepted",
                    status: parent.accepted,
                    reason: parent.accepted_reason,
                    message: if parent.accepted {
                        "Route accepted by sozu-gateway".to_string()
                    } else {
                        problems_message(&parent_problems, "Route does not bind to this parent")
                    },
                },
                Desired {
                    type_: "ResolvedRefs",
                    status: parent.resolved_refs,
                    reason: parent.resolved_refs_reason,
                    message: if parent.resolved_refs {
                        "All backend references resolved".to_string()
                    } else {
                        problems_message(
                            &parent_problems,
                            "One or more backend references could not be resolved",
                        )
                    },
                },
            ],
            existing.and_then(|p| p.conditions.as_deref()),
            generation,
        );
        parents.push(HttpRouteStatusParents {
            conditions: Some(conditions),
            controller_name: controller_name.to_string(),
            parent_ref: HttpRouteStatusParentsParentRef {
                group: Some(GW_GROUP.to_string()),
                kind: Some("Gateway".to_string()),
                name: parent.gateway_name.clone(),
                namespace: Some(parent.gateway_namespace.clone()),
                port: None,
                section_name: None,
            },
        });
    }

    // Skip the write when the full parents list is unchanged (loop-safety).
    if serde_json::to_value(&current_parents).ok() == serde_json::to_value(&parents).ok() {
        return Ok(());
    }
    let patch = json!({ "status": { "parents": parents } });
    api.patch_status(&route.name, &PatchParams::default(), &Patch::Merge(&patch))
        .await?;
    debug!(namespace = %route.namespace, name = %route.name, "HTTPRoute status updated");
    Ok(())
}

// ---- Ingress status (.status.loadBalancer.ingress) -------------------------

/// Map the publish Service's load-balancer address(es) into the shape an Ingress
/// status expects. Pure, so it is unit-tested without a cluster.
///
/// The result is sorted by `(ip, hostname)` so the order is independent of the
/// Service status's array order. The loop-safety guard in [`write_one_ingress`]
/// compares element-wise, so without this a provider that re-orders its
/// `loadBalancer.ingress` between reads would cause endless no-op re-patches.
pub(crate) fn lb_points(svc: &Service) -> Vec<IngressLoadBalancerIngress> {
    let mut points: Vec<IngressLoadBalancerIngress> = svc
        .status
        .as_ref()
        .and_then(|s| s.load_balancer.as_ref())
        .and_then(|lb| lb.ingress.as_ref())
        .map(|points| {
            points
                .iter()
                .map(|p| IngressLoadBalancerIngress {
                    hostname: p.hostname.clone(),
                    ip: p.ip.clone(),
                    ports: None,
                })
                .collect()
        })
        .unwrap_or_default();
    points.sort_by(|a, b| (&a.ip, &a.hostname).cmp(&(&b.ip, &b.hostname)));
    points
}

/// Publish the gateway's external address into each managed Ingress's
/// `.status.loadBalancer.ingress`. Loop-safe (skips no-op patches) and
/// best-effort. Does nothing when there is no address yet, so a still-pending
/// LoadBalancer never clears an Ingress's status.
pub async fn write_ingress_status(
    client: &Client,
    ingresses: &[IngressResult],
    points: &[IngressLoadBalancerIngress],
) {
    if points.is_empty() {
        return;
    }
    for r in ingresses {
        if let Err(e) = write_one_ingress(client, &r.namespace, &r.name, points).await {
            warn!(namespace = %r.namespace, name = %r.name, error = %e, "failed to write Ingress status");
        }
    }
}

async fn write_one_ingress(
    client: &Client,
    namespace: &str,
    name: &str,
    points: &[IngressLoadBalancerIngress],
) -> Result<(), kube::Error> {
    let api: Api<Ingress> = Api::namespaced(client.clone(), namespace);
    let current = api.get(name).await?;
    let cur = current
        .status
        .as_ref()
        .and_then(|s| s.load_balancer.as_ref())
        .and_then(|lb| lb.ingress.as_deref())
        .unwrap_or_default();
    if cur == points {
        return Ok(()); // already published — skip to stay loop-safe
    }
    let patch = json!({ "status": { "loadBalancer": { "ingress": points } } });
    api.patch_status(name, &PatchParams::default(), &Patch::Merge(&patch))
        .await?;
    debug!(namespace = %namespace, name = %name, "Ingress status updated");
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn problems_message_is_deterministic_deduped_and_capped() {
        assert_eq!(problems_message(&[], "fallback"), "fallback");

        // Order-insensitive and deduped: the message participates in
        // lastTransitionTime reuse, so it must not flap across reconciles.
        let a = Problem::ServiceNotFound {
            service: "z".into(),
        };
        let b = Problem::ServiceNotFound {
            service: "a".into(),
        };
        let one = problems_message(&[&a, &b, &a], "");
        let two = problems_message(&[&b, &a, &b], "");
        assert_eq!(one, two);
        assert_eq!(one.matches("\"z\"").count(), 1, "duplicates collapse");

        let many: Vec<Problem> = (0..8)
            .map(|i| Problem::ServiceNotFound {
                service: format!("s{i}"),
            })
            .collect();
        let refs: Vec<&Problem> = many.iter().collect();
        assert!(problems_message(&refs, "").ends_with("(+3 more)"));
    }

    fn svc_with_ips(ips: &[&str]) -> Service {
        let ingress: Vec<_> = ips.iter().map(|ip| json!({ "ip": ip })).collect();
        serde_json::from_value(json!({
            "metadata": { "name": "gw", "namespace": "sozu-system" },
            "status": { "loadBalancer": { "ingress": ingress } }
        }))
        .unwrap()
    }

    #[test]
    fn lb_points_extracts_ip_and_hostname() {
        let svc: Service = serde_json::from_value(json!({
            "metadata": { "name": "gw", "namespace": "sozu-system" },
            "status": { "loadBalancer": { "ingress": [
                { "ip": "1.2.3.4" },
                { "hostname": "lb.example.com" }
            ] } }
        }))
        .unwrap();
        let pts = lb_points(&svc);
        assert_eq!(pts.len(), 2);
        assert!(pts.iter().any(|p| p.ip.as_deref() == Some("1.2.3.4")));
        assert!(pts
            .iter()
            .any(|p| p.hostname.as_deref() == Some("lb.example.com")));
    }

    #[test]
    fn lb_points_order_is_canonical() {
        // Same address set in two different Service orders must map to the same
        // (sorted) Vec, so the loop-safety comparison never flips on reorder.
        let a = lb_points(&svc_with_ips(&["10.0.0.2", "10.0.0.1"]));
        let b = lb_points(&svc_with_ips(&["10.0.0.1", "10.0.0.2"]));
        assert_eq!(a, b);
        assert_eq!(a[0].ip.as_deref(), Some("10.0.0.1"));
    }

    #[test]
    fn lb_points_empty_when_no_loadbalancer_status() {
        let svc: Service = serde_json::from_value(json!({
            "metadata": { "name": "gw", "namespace": "sozu-system" }
        }))
        .unwrap();
        assert!(lb_points(&svc).is_empty());
    }

    #[test]
    fn gateway_addresses_typed_from_lb() {
        let svc: Service = serde_json::from_value(json!({
            "metadata": { "name": "gw", "namespace": "sozu-system" },
            "status": { "loadBalancer": { "ingress": [
                { "ip": "1.2.3.4" },
                { "hostname": "lb.example.com" }
            ] } }
        }))
        .unwrap();
        let addrs = gateway_addresses(&svc);
        assert_eq!(addrs.len(), 2);
        assert!(addrs
            .iter()
            .any(|a| a.r#type.as_deref() == Some("IPAddress") && a.value == "1.2.3.4"));
        assert!(addrs
            .iter()
            .any(|a| a.r#type.as_deref() == Some("Hostname") && a.value == "lb.example.com"));
    }
}
