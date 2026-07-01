//! Sōzu gateway controller binary.
//!
//! A singleton controller: it maintains reflector caches for Ingress,
//! IngressClass, Service, EndpointSlice and Secret; any relevant change (or a
//! periodic resync) triggers one debounced **global** reconcile that rebuilds
//! the whole desired state from the caches, diffs it against the last-applied
//! shadow `ConfigState`, and pushes only the minimal mutations to Sōzu.
//!
//! The pure crates do the work: `builder` (objects → IR), `translator`
//! (IR → diff → commands), `sozu-agent` (socket I/O). This file is just the
//! kube-rs wiring and the reconcile loop.

use std::hash::Hash;
use std::net::SocketAddr;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Arc;
use std::time::Duration;

use anyhow::{Context, Result};
use clap::Parser;
use futures::StreamExt;
use k8s_openapi::api::core::v1::{ConfigMap, Secret, Service};
use k8s_openapi::api::discovery::v1::EndpointSlice;
use k8s_openapi::api::networking::v1::{Ingress, IngressClass};
use kube::api::ListParams;
use kube::runtime::reflector::{store::Writer, Store};
use kube::runtime::{reflector, watcher, WatchStreamExt};
use kube::{Api, Client, Resource};
use serde::de::DeserializeOwned;
use sozu_gw_ir::Ir;
use tokio::sync::mpsc;
use tracing::{debug, error, info, warn};
use tracing_subscriber::EnvFilter;

use sozu_gw_agent::SozuAgentHandle;
use sozu_gw_builder::{build, BuildConfig, Inputs};
use sozu_gw_gateway_api::{Gateway, GatewayClass, HttpRoute, ReferenceGrant};
use sozu_gw_translator as tr;

mod health;
mod metrics;
mod shadow;
mod status;

const DEFAULT_CLASS_ANNOTATION: &str = "ingressclass.kubernetes.io/is-default-class";

#[derive(Parser, Debug, Clone)]
#[command(
    name = "sozu-gw-controller",
    about = "Sōzu-based Ingress + Gateway API controller"
)]
struct Args {
    /// IngressClass name we own.
    #[arg(long, env = "SOZU_GW_CLASS", default_value = "sozu")]
    class_name: String,
    /// GatewayClass controllerName we own (Gateway API).
    #[arg(
        long,
        env = "SOZU_GW_CONTROLLER",
        default_value = "sozu.io/gateway-controller"
    )]
    controller_name: String,
    /// Path to the Sōzu command socket.
    #[arg(long, env = "SOZU_GW_SOCKET", default_value = "/run/sozu/sozu.sock")]
    socket: String,
    /// HTTP listener address declared in Sōzu's config.toml.
    #[arg(long, env = "SOZU_GW_HTTP_LISTENER", default_value = "0.0.0.0:80")]
    http_listener: SocketAddr,
    /// HTTPS listener address declared in Sōzu's config.toml.
    #[arg(long, env = "SOZU_GW_HTTPS_LISTENER", default_value = "0.0.0.0:443")]
    https_listener: SocketAddr,
    /// Debounce window: coalesce bursts of watch events before reconciling.
    #[arg(long, env = "SOZU_GW_DEBOUNCE_MS", default_value = "500")]
    debounce_ms: u64,
    /// Periodic full resync interval (self-heals any drift).
    #[arg(long, env = "SOZU_GW_RESYNC_SECS", default_value = "60")]
    resync_secs: u64,
    /// Publish this Service's LoadBalancer address into managed Ingresses'
    /// `.status` (format `namespace/name`). Unset = don't write Ingress status.
    /// Requires the `ingresses/status` RBAC (Helm `rbac.allowStatusWrites`).
    #[arg(long, env = "SOZU_GW_PUBLISH_SERVICE")]
    publish_service: Option<String>,
    /// Bind address for the health endpoints (`/healthz`, `/readyz`).
    #[arg(long, env = "SOZU_GW_HEALTH_LISTEN", default_value = "0.0.0.0:8081")]
    health_listen: SocketAddr,
    /// Bind address for the Prometheus `/metrics` endpoint (pulls Sōzu's
    /// metrics over the command socket on each scrape). Unset disables it.
    #[arg(long, env = "SOZU_GW_METRICS_LISTEN")]
    metrics_listen: Option<SocketAddr>,
    /// File on the shared volume where the last-applied state is persisted, so a
    /// controller-only restart resumes from it (and prunes orphaned Sōzu state)
    /// instead of re-applying everything. Empty disables persistence.
    #[arg(
        long,
        env = "SOZU_GW_SHADOW_FILE",
        default_value = "/run/sozu/shadow.json"
    )]
    shadow_file: String,
    /// ConfigMap (`namespace/name`) mapping TCP ports to Services, ingress-nginx
    /// style (`"<port>": "<ns>/<svc>:<port>"`). Unset disables TCP L4.
    #[arg(long, env = "SOZU_GW_TCP_SERVICES")]
    tcp_services_configmap: Option<String>,
    /// Same as `--tcp-services-configmap`, for UDP.
    #[arg(long, env = "SOZU_GW_UDP_SERVICES")]
    udp_services_configmap: Option<String>,
}

/// Reflector read handles for every watched resource type.
struct Stores {
    ingresses: Store<Ingress>,
    ingress_classes: Store<IngressClass>,
    services: Store<Service>,
    endpointslices: Store<EndpointSlice>,
    secrets: Store<Secret>,
    /// ConfigMaps (only watched when L4 tcp/udp-services are configured).
    config_maps: Store<ConfigMap>,
    // Gateway API (Phase 2).
    gateway_classes: Store<GatewayClass>,
    gateways: Store<Gateway>,
    http_routes: Store<HttpRoute>,
    reference_grants: Store<ReferenceGrant>,
}

/// Spawn a watcher+reflector that keeps `writer`'s store fresh and pings `tx`
/// on every event. Returns the read store.
fn spawn_watch<K>(api: Api<K>, writer: Writer<K>, tx: mpsc::Sender<()>, kind: &'static str)
where
    K: Resource + Clone + DeserializeOwned + std::fmt::Debug + Send + Sync + 'static,
    K::DynamicType: Default + Eq + Hash + Clone + std::fmt::Debug + Unpin,
{
    let stream = watcher(api, watcher::Config::default())
        .default_backoff()
        .reflect(writer)
        .touched_objects();
    tokio::spawn(async move {
        futures::pin_mut!(stream);
        loop {
            match stream.next().await {
                Some(Ok(_)) => {
                    let _ = tx.try_send(());
                }
                Some(Err(e)) => warn!(watch = kind, error = %e, "watch error (will retry)"),
                None => {
                    // The watcher's own backoff means a healthy stream never
                    // ends; if it does, fail fast so Kubernetes restarts us
                    // rather than silently going blind to this resource.
                    error!(
                        watch = kind,
                        "watch stream ended unexpectedly; exiting for restart"
                    );
                    std::process::exit(1);
                }
            }
        }
    });
}

/// Is our IngressClass marked as the cluster default?
fn class_is_default(stores: &Stores, class_name: &str) -> bool {
    stores.ingress_classes.state().iter().any(|ic| {
        ic.metadata.name.as_deref() == Some(class_name)
            && ic
                .metadata
                .annotations
                .as_ref()
                .and_then(|a| a.get(DEFAULT_CLASS_ANNOTATION))
                .map(|v| v == "true")
                .unwrap_or(false)
    })
}

fn collect<K>(store: &Store<K>) -> Vec<K>
where
    K: Resource + Clone + 'static,
    K::DynamicType: Hash + Eq + Clone,
{
    store.state().iter().map(|a| (**a).clone()).collect()
}

/// Find a `namespace/name` ConfigMap in the cache (L4 tcp/udp-services).
fn lookup_configmap(store: &Store<ConfigMap>, spec: &Option<String>) -> Option<ConfigMap> {
    let (ns, name) = spec.as_ref()?.split_once('/')?;
    store
        .state()
        .iter()
        .find(|cm| {
            cm.metadata.namespace.as_deref() == Some(ns)
                && cm.metadata.name.as_deref() == Some(name)
        })
        .map(|cm| (**cm).clone())
}

/// Latch readiness on the first successful reconcile (logging the transition).
/// Never unset: a later transient failure must not pull a serving Pod out of the
/// Service's endpoints.
fn mark_ready(ready: &AtomicBool) {
    if !ready.swap(true, Ordering::Relaxed) {
        info!("controller ready: first reconcile complete");
    }
}

/// Are the Gateway API CRDs installed? Probed by a tiny list against
/// `GatewayClass`; a `NotFound`/discovery error means the CRDs are absent.
async fn gateway_api_available(client: &Client) -> bool {
    let api: Api<GatewayClass> = Api::all(client.clone());
    match api.list(&ListParams::default().limit(1)).await {
        Ok(_) => true,
        Err(e) => {
            debug!(error = %e, "Gateway API not available");
            false
        }
    }
}

/// One global reconcile: caches → IR → diff → apply. Updates `shadow` (the
/// last-applied IR) only on a successful apply, so a failed push is retried from
/// the same baseline.
async fn reconcile(
    args: &Args,
    client: &Client,
    stores: &Stores,
    agent: &SozuAgentHandle,
    shadow: &mut Ir,
) -> Result<()> {
    let cfg = BuildConfig {
        class_name: args.class_name.clone(),
        class_is_default: class_is_default(stores, &args.class_name),
        controller_name: args.controller_name.clone(),
        http_listener: args.http_listener,
        https_listener: args.https_listener,
    };
    let inputs = Inputs {
        ingresses: collect(&stores.ingresses),
        services: collect(&stores.services),
        endpointslices: collect(&stores.endpointslices),
        secrets: collect(&stores.secrets),
        gateway_classes: collect(&stores.gateway_classes),
        gateways: collect(&stores.gateways),
        http_routes: collect(&stores.http_routes),
        reference_grants: collect(&stores.reference_grants),
        tcp_services: lookup_configmap(&stores.config_maps, &args.tcp_services_configmap),
        udp_services: lookup_configmap(&stores.config_maps, &args.udp_services_configmap),
    };

    let out = build(&cfg, &inputs);
    for r in &out.results {
        if !r.problems.is_empty() {
            warn!(namespace = %r.namespace, name = %r.name, problems = ?r.problems, "ingress has problems");
        }
    }
    for g in &out.gateways {
        if !g.problems.is_empty() {
            warn!(namespace = %g.namespace, name = %g.name, problems = ?g.problems, "gateway has problems");
        }
    }
    for route in &out.routes {
        for parent in &route.parents {
            if !parent.problems.is_empty() {
                warn!(namespace = %route.namespace, name = %route.name, gateway = %parent.gateway_name, problems = ?parent.problems, "httproute has problems");
            }
        }
    }
    for r in &out.l4_results {
        if !r.problems.is_empty() {
            warn!(protocol = %r.protocol, port = r.listen_port, target = %r.target, problems = ?r.problems, "l4 service has problems");
        }
    }

    let requests = tr::reconcile(shadow, &out.ir).context("translate IR to commands")?;
    let mut applied = false;
    if requests.is_empty() {
        debug!("reconcile: no socket changes");
    } else {
        info!(
            clusters = out.ir.clusters.len(),
            backends = out.ir.backends.len(),
            frontends = out.ir.frontends.len(),
            certificates = out.ir.certificates.len(),
            requests = requests.len(),
            "applying changes to sozu"
        );
        // Bound the apply so a wedged Sōzu socket surfaces as a retryable error
        // instead of stalling the reconcile loop indefinitely.
        tokio::time::timeout(Duration::from_secs(60), agent.apply(requests))
            .await
            .context("timed out applying requests to sozu")?
            .context("apply requests to sozu")?;
        applied = true;
    }

    // Report Gateway API status (best-effort; never fails the reconcile). It is
    // loop-safe: a no-op patch is skipped, so our own writes don't re-trigger.
    // Resolve our own LoadBalancer Service once: its address is published into
    // both Ingress `.status` and Gateway `.status.addresses` (what external-dns
    // consumes). Best-effort + loop-safe (writes skipped when already current).
    let publish_svc = args
        .publish_service
        .as_deref()
        .and_then(|s| s.split_once('/'))
        .and_then(|(ns, name)| {
            inputs.services.iter().find(|s| {
                s.metadata.namespace.as_deref() == Some(ns)
                    && s.metadata.name.as_deref() == Some(name)
            })
        });
    let gw_addresses = publish_svc
        .map(status::gateway_addresses)
        .unwrap_or_default();

    status::write_status(
        client,
        &args.controller_name,
        &out.gateway_classes,
        &out.gateways,
        &out.routes,
        &gw_addresses,
    )
    .await;

    let lb_points = publish_svc.map(status::lb_points).unwrap_or_default();
    status::write_ingress_status(client, &out.results, &lb_points).await;

    // Shadow advances only on a successful socket apply. On failure it stays at
    // the previous applied IR. The emitted requests are not all idempotent, so
    // re-diffing from the unchanged shadow converges thanks to Sōzu's upsert
    // semantics for clusters/backends plus the agent's handling of the rest:
    // already-gone teardowns are tolerated, duplicate frontend adds repaired
    // (remove + re-add on the same route key).
    if applied {
        *shadow = out.ir;
        // Persist the new baseline so a controller-only restart resumes from it.
        shadow::persist(&args.shadow_file, shadow);
    }
    Ok(())
}

#[tokio::main]
async fn main() -> Result<()> {
    tracing_subscriber::fmt()
        .with_env_filter(
            EnvFilter::try_from_default_env().unwrap_or_else(|_| EnvFilter::new("info")),
        )
        .init();

    let args = Args::parse();
    info!(?args, "starting sozu gateway controller");

    if let Some(ps) = &args.publish_service {
        let valid = ps
            .split_once('/')
            .is_some_and(|(ns, name)| !ns.is_empty() && !name.is_empty() && !name.contains('/'));
        if !valid {
            warn!(publish_service = %ps, "--publish-service must be namespace/name; Ingress status will not be written");
        }
    }

    // Health endpoints come up immediately: /healthz (liveness) is green now,
    // while /readyz (readiness) stays 503 until the first reconcile has
    // programmed Sōzu, so the Pod takes no traffic during the cold-start gap.
    let ready = Arc::new(AtomicBool::new(false));
    health::spawn(args.health_listen, ready.clone());

    let client = Client::try_default()
        .await
        .context("create kube client (in-cluster or kubeconfig)")?;
    let agent = SozuAgentHandle::spawn(&args.socket).context("spawn sozu-agent")?;

    // Optional Prometheus `/metrics`: each scrape pulls Sōzu's aggregated
    // metrics over the same command socket and renders them. Best-effort and
    // independent of routing — a bind failure never affects reconciliation.
    if let Some(addr) = args.metrics_listen {
        metrics::spawn(addr, agent.clone());
    }

    // One signal channel fed by every watcher.
    let (tx, mut rx) = mpsc::channel::<()>(64);

    let (ingresses, w) = reflector::store();
    spawn_watch::<Ingress>(Api::all(client.clone()), w, tx.clone(), "ingress");
    let (ingress_classes, w) = reflector::store();
    spawn_watch::<IngressClass>(Api::all(client.clone()), w, tx.clone(), "ingressclass");
    let (services, w) = reflector::store();
    spawn_watch::<Service>(Api::all(client.clone()), w, tx.clone(), "service");
    let (endpointslices, w) = reflector::store();
    spawn_watch::<EndpointSlice>(Api::all(client.clone()), w, tx.clone(), "endpointslice");
    let (secrets, w) = reflector::store();
    spawn_watch::<Secret>(Api::all(client.clone()), w, tx.clone(), "secret");

    // ConfigMaps are only watched when L4 (tcp/udp-services) is configured, so a
    // cluster not using L4 pays no ConfigMap-watch cost.
    let (config_maps, cm_w) = reflector::store();
    if args.tcp_services_configmap.is_some() || args.udp_services_configmap.is_some() {
        info!("L4 services configured; watching ConfigMaps");
        spawn_watch::<ConfigMap>(Api::all(client.clone()), cm_w, tx.clone(), "configmap");
    } else {
        drop(cm_w);
    }

    // Gateway API CRDs are optional. Only watch them when they are installed, so
    // an Ingress-only cluster runs cleanly instead of logging watch errors.
    let (gateway_classes, gc_w) = reflector::store();
    let (gateways, gw_w) = reflector::store();
    let (http_routes, hr_w) = reflector::store();
    let (reference_grants, rg_w) = reflector::store();
    if gateway_api_available(&client).await {
        info!("Gateway API detected; watching gateway.networking.k8s.io resources");
        spawn_watch::<GatewayClass>(Api::all(client.clone()), gc_w, tx.clone(), "gatewayclass");
        spawn_watch::<Gateway>(Api::all(client.clone()), gw_w, tx.clone(), "gateway");
        spawn_watch::<HttpRoute>(Api::all(client.clone()), hr_w, tx.clone(), "httproute");
        spawn_watch::<ReferenceGrant>(Api::all(client.clone()), rg_w, tx.clone(), "referencegrant");
    } else {
        info!("Gateway API CRDs not found; running in Ingress-only mode");
        drop((gc_w, gw_w, hr_w, rg_w));
    }

    let stores = Stores {
        ingresses,
        ingress_classes,
        services,
        endpointslices,
        secrets,
        config_maps,
        gateway_classes,
        gateways,
        http_routes,
        reference_grants,
    };

    // Wait for the caches to fill so the first reconcile sees a complete picture.
    // Bounded so a wedged/permission-denied watcher surfaces as a clear failure
    // (CrashLoopBackOff) instead of hanging forever.
    info!("waiting for informer caches to sync...");
    let sync = async {
        tokio::try_join!(
            stores.ingresses.wait_until_ready(),
            stores.ingress_classes.wait_until_ready(),
            stores.services.wait_until_ready(),
            stores.endpointslices.wait_until_ready(),
            stores.secrets.wait_until_ready(),
        )
    };
    tokio::time::timeout(Duration::from_secs(120), sync)
        .await
        .context("timed out waiting for informer caches to sync (check RBAC)")?
        .context("informer cache writer dropped before becoming ready")?;
    info!("caches synced");

    // Shadow of the last successfully-applied IR. Resumed from the shared volume
    // when Sōzu still holds its state (a controller-only restart), so the first
    // reconcile prunes orphans instead of re-adding everything; otherwise empty,
    // so a fresh/just-restarted Sōzu gets the full state.
    let probe_file = format!("{}.probe", args.shadow_file);
    let mut shadow = shadow::load_initial(&agent, &args.shadow_file, &probe_file).await;

    let debounce = Duration::from_millis(args.debounce_ms);
    let mut resync = tokio::time::interval(Duration::from_secs(args.resync_secs));
    resync.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Skip);

    // Graceful shutdown on the signals Kubernetes uses on Pod termination, so we
    // stop cleanly within the grace period instead of being SIGKILLed.
    use tokio::signal::unix::{signal, SignalKind};
    let mut sigterm = signal(SignalKind::terminate()).context("install SIGTERM handler")?;
    let mut sigint = signal(SignalKind::interrupt()).context("install SIGINT handler")?;

    // Initial reconcile (full apply). Readiness latches once this succeeds.
    match reconcile(&args, &client, &stores, &agent, &mut shadow).await {
        Ok(()) => mark_ready(&ready),
        Err(e) => error!(error = ?e, "initial reconcile failed; will retry"),
    }

    loop {
        // Wait for a change signal or the resync tick.
        tokio::select! {
            maybe = rx.recv() => {
                if maybe.is_none() { warn!("all watchers gone; exiting"); break; }
                // Debounce: let a burst settle, then drain the queue.
                tokio::time::sleep(debounce).await;
                while rx.try_recv().is_ok() {}
            }
            _ = resync.tick() => {
                debug!("periodic resync");
            }
            _ = sigterm.recv() => { info!("SIGTERM received; shutting down"); break; }
            _ = sigint.recv() => { info!("SIGINT received; shutting down"); break; }
        }

        match reconcile(&args, &client, &stores, &agent, &mut shadow).await {
            Ok(()) => mark_ready(&ready),
            Err(e) => error!(error = ?e, "reconcile failed; will retry on next event/resync"),
        }
    }

    Ok(())
}
