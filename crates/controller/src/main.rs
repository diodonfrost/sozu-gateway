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

use std::collections::BTreeSet;
use std::hash::Hash;
use std::net::SocketAddr;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, RwLock};
use std::time::Duration;

use anyhow::{Context, Result};
use clap::Parser;
use futures::StreamExt;
use k8s_openapi::api::core::v1::{ConfigMap, Secret, Service};
use k8s_openapi::api::discovery::v1::EndpointSlice;
use k8s_openapi::api::networking::v1::{Ingress, IngressClass};
use kube::api::ListParams;
use kube::runtime::reflector::{
    store::{Writer, WriterDropped},
    Lookup, Store,
};
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

mod events;
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
    /// Externally advertised port for HTTP Gateway listeners — what a
    /// Gateway's `listener.port` declares and clients connect to (the
    /// LoadBalancer Service's exposed port), as opposed to the pod-level
    /// `--http-listener` bind.
    #[arg(long, env = "SOZU_GW_GATEWAY_HTTP_PORT", default_value = "80")]
    gateway_http_port: u16,
    /// Externally advertised port for HTTPS Gateway listeners (see
    /// `--gateway-http-port`).
    #[arg(long, env = "SOZU_GW_GATEWAY_HTTPS_PORT", default_value = "443")]
    gateway_https_port: u16,
    /// Debounce window: coalesce bursts of watch events before reconciling.
    #[arg(long, env = "SOZU_GW_DEBOUNCE_MS", default_value = "500")]
    debounce_ms: u64,
    /// Periodic full resync interval in seconds (self-heals any drift). `0`
    /// disables the periodic resync; Sōzu-restart detection then only runs
    /// when the command socket reconnects, not on a schedule.
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
    /// Write Gateway API status (GatewayClass/Gateway/HTTPRoute conditions).
    /// On by default — conditions are the API's UX — but can be disabled for
    /// least-privilege deployments without the `*/status` RBAC grants (Helm
    /// `rbac.allowGatewayStatusWrites=false`), where every write would 403.
    #[arg(
        long,
        env = "SOZU_GW_GATEWAY_STATUS_WRITES",
        default_value_t = true,
        action = clap::ArgAction::Set
    )]
    gateway_status_writes: bool,
}

/// Reflector read handles for every watched resource type.
struct Stores {
    ingresses: Store<Ingress>,
    ingress_classes: Store<IngressClass>,
    services: Store<Service>,
    endpointslices: Store<EndpointSlice>,
    secrets: Store<Secret>,
    /// ConfigMap caches, one per watched namespace (only the namespaces named
    /// by the L4 tcp/udp-services specs are watched; empty when L4 is off).
    config_maps: Vec<Store<ConfigMap>>,
    // Gateway API (Phase 2).
    gateway_classes: Store<GatewayClass>,
    gateways: Store<Gateway>,
    http_routes: Store<HttpRoute>,
    reference_grants: Store<ReferenceGrant>,
}

/// Spawn a watcher+reflector that keeps `writer`'s store fresh and pings `tx`
/// on every event.
fn spawn_watch<K>(
    api: Api<K>,
    cfg: watcher::Config,
    writer: Writer<K>,
    tx: mpsc::Sender<()>,
    kind: &'static str,
) where
    K: Resource + Clone + DeserializeOwned + std::fmt::Debug + Send + Sync + 'static,
    K::DynamicType: Default + Eq + Hash + Clone + std::fmt::Debug + Unpin,
{
    spawn_watch_filtered(api, cfg, writer, tx, kind, |_: &K| true)
}

/// [`spawn_watch`] with a ping predicate: the reflector cache is kept fresh
/// for **every** event — the filter never touches `.reflect` — but only
/// objects `relevant` accepts wake the reconcile loop. Used by the
/// EndpointSlice watch, where churn from unrelated workloads dominates.
fn spawn_watch_filtered<K, F>(
    api: Api<K>,
    cfg: watcher::Config,
    writer: Writer<K>,
    tx: mpsc::Sender<()>,
    kind: &'static str,
    relevant: F,
) where
    K: Resource + Clone + DeserializeOwned + std::fmt::Debug + Send + Sync + 'static,
    K::DynamicType: Default + Eq + Hash + Clone + std::fmt::Debug + Unpin,
    F: Fn(&K) -> bool + Send + 'static,
{
    let stream = watcher(api, cfg)
        .default_backoff()
        .reflect(writer)
        .touched_objects();
    tokio::spawn(async move {
        futures::pin_mut!(stream);
        loop {
            match stream.next().await {
                Some(Ok(obj)) => {
                    if relevant(&obj) {
                        let _ = tx.try_send(());
                    }
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

/// Should an EndpointSlice event wake the reconcile loop? Only when its
/// Service (`kubernetes.io/service-name` label + namespace) is one the last
/// build referenced — resolved or not. Endpoint churn from unrelated
/// workloads is the dominant wakeup source on a busy cluster, and every
/// wakeup is a full rebuild.
///
/// An empty set passes everything: before the first build has populated it,
/// a missed wakeup is an outage risk while a spurious one only costs CPU
/// (and a cluster whose build genuinely references nothing rebuilds an empty
/// state, which is cheap). A slice without the service-name label never
/// pings — the builder cannot attribute it to any Service, so it can never
/// change the build output.
fn slice_pings(referenced: &BTreeSet<String>, slice: &EndpointSlice) -> bool {
    if referenced.is_empty() {
        return true;
    }
    sozu_gw_builder::slice_service_key(slice).is_some_and(|key| referenced.contains(&key))
}

/// Await a store's readiness only when its watcher was actually spawned.
///
/// Optional features (L4 ConfigMaps, Gateway API) drop their `Writer` when
/// disabled, and `wait_until_ready` on such a store fails immediately; a store
/// that is never watched is trivially "ready" instead. For a watched store this
/// is the plain readiness wait, so the sync gate covers *every* cache the first
/// reconcile will read — skipping one would let a resumed shadow diff against a
/// half-built IR and tear down live routes.
async fn ready_when<K>(watched: bool, store: &Store<K>) -> Result<(), WriterDropped>
where
    K: Lookup + Clone + 'static,
    K::DynamicType: Eq + Hash + Clone,
{
    if watched {
        store.wait_until_ready().await
    } else {
        Ok(())
    }
}

/// Interpret the configured resync interval: `0` means "disabled" (a zero
/// `tokio::time::interval` would panic, and disabling the periodic resync is
/// the only sensible reading of an explicit `SOZU_GW_RESYNC_SECS=0`).
fn resync_period(secs: u64) -> Option<Duration> {
    (secs != 0).then(|| Duration::from_secs(secs))
}

/// Build the periodic resync interval. Unlike a raw `interval()`, whose first
/// tick completes immediately (which would re-run a redundant reconcile right
/// after the initial one), the first tick lands one full period after startup.
fn resync_interval(period: Duration) -> tokio::time::Interval {
    let mut interval = tokio::time::interval(period);
    interval.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Skip);
    interval.reset();
    interval
}

/// Tick the optional resync interval; when resync is disabled, pend forever so
/// the `select!` arm simply never fires.
async fn maybe_tick(interval: Option<&mut tokio::time::Interval>) {
    match interval {
        Some(interval) => {
            interval.tick().await;
        }
        None => std::future::pending::<()>().await,
    }
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

/// The distinct namespaces named by the L4 `namespace/name` ConfigMap specs.
/// These are the only namespaces the controller watches ConfigMaps in — the
/// RBAC grant is a namespaced Role, not a cluster-wide one, so `Api::all`
/// would be both over-privileged and forbidden. A malformed spec (no `/`)
/// yields no namespace; [`lookup_configmap`] could never resolve it anyway.
fn l4_namespaces(specs: [&Option<String>; 2]) -> BTreeSet<String> {
    specs
        .into_iter()
        .flatten()
        .filter_map(|spec| spec.split_once('/'))
        .map(|(ns, _)| ns.to_string())
        .collect()
}

/// Find a `namespace/name` ConfigMap in the per-namespace caches (L4
/// tcp/udp-services).
fn lookup_configmap(stores: &[Store<ConfigMap>], spec: &Option<String>) -> Option<ConfigMap> {
    let (ns, name) = spec.as_ref()?.split_once('/')?;
    stores
        .iter()
        .flat_map(|store| store.state())
        .find(|cm| {
            cm.metadata.namespace.as_deref() == Some(ns)
                && cm.metadata.name.as_deref() == Some(name)
        })
        .map(|cm| (*cm).clone())
}

/// Latch readiness on the first successful reconcile (logging the transition).
/// Never unset: a later transient failure must not pull a serving Pod out of the
/// Service's endpoints.
fn mark_ready(ready: &AtomicBool) {
    if !ready.swap(true, Ordering::Relaxed) {
        info!("controller ready: first reconcile complete");
    }
}

/// The Gateway API kinds the controller watches. Gateway mode requires *all*
/// of them: watching a missing kind would never sync its cache, and the sync
/// gate would kill the process — a partial install must run Ingress-only, not
/// crash-loop.
const GATEWAY_API_KINDS: [&str; 4] = ["GatewayClass", "Gateway", "HTTPRoute", "ReferenceGrant"];

/// Are the Gateway API CRDs installed? Probed by a tiny list against **every**
/// kind the controller watches — a partial install (e.g. GatewayClass served
/// but ReferenceGrant absent) is a real cluster state and must read as
/// Ingress-only (with a warning naming the missing kinds), not as gateway
/// mode. Only a 404 from the apiserver (the CRD's group/kind is not served,
/// see [`gateway_crds_absent`]) means "absent"; any other failure — an
/// apiserver hiccup, RBAC not yet propagated — is propagated so startup fails
/// fast and
/// Kubernetes restarts us, instead of silently locking the whole process into
/// Ingress-only mode for its lifetime.
async fn gateway_api_available(client: &Client) -> Result<bool> {
    let served = [
        crd_served::<GatewayClass>(client).await?,
        crd_served::<Gateway>(client).await?,
        crd_served::<HttpRoute>(client).await?,
        crd_served::<ReferenceGrant>(client).await?,
    ];
    let missing = missing_gateway_crds(served);
    if missing.is_empty() {
        Ok(true)
    } else {
        if missing.len() == GATEWAY_API_KINDS.len() {
            // No Gateway API at all: the ordinary Ingress-only cluster.
            debug!("Gateway API CRDs not installed");
        } else {
            warn!(
                ?missing,
                "partial Gateway API install: some watched CRDs are not served; \
                 running in Ingress-only mode until all of them are installed"
            );
        }
        Ok(false)
    }
}

/// Probe one watched kind with a tiny list: `Ok(true)` = served, `Ok(false)` =
/// the apiserver does not serve it (`NotFound`), `Err` = anything else (fail
/// fast).
async fn crd_served<K>(client: &Client) -> Result<bool>
where
    K: Resource + Clone + DeserializeOwned + std::fmt::Debug,
    K::DynamicType: Default,
{
    let api: Api<K> = Api::all(client.clone());
    match api.list(&ListParams::default().limit(1)).await {
        Ok(_) => Ok(true),
        Err(e) if gateway_crds_absent(&e) => Ok(false),
        Err(e) => Err(e).context("probe Gateway API availability"),
    }
}

/// Pure classifier: which watched Gateway API kinds are missing, given the
/// per-kind probe results (in [`GATEWAY_API_KINDS`] order). Any missing kind
/// forces Ingress-only mode.
fn missing_gateway_crds(served: [bool; 4]) -> Vec<&'static str> {
    GATEWAY_API_KINDS
        .iter()
        .zip(served)
        .filter_map(|(kind, served)| (!served).then_some(*kind))
        .collect()
}

/// Classify the probe error: a 404 from the apiserver (what the list returns
/// when the CRD's group/kind is not served) means the CRD is absent. Matched
/// by HTTP code, not only by the parsed `NotFound` reason: managed clusters
/// that front the apiserver with an HTTP router can answer an unserved
/// group's path with a plain-text `404 page not found` body, which
/// kube-client cannot parse into a typed `Status` — `reason` is then a
/// synthetic parse-failure marker but `code` is still 404. A 404 on a
/// collection list is never transient, so everything else stays fail-fast.
fn gateway_crds_absent(err: &kube::Error) -> bool {
    matches!(err, kube::Error::Api(status) if status.is_not_found() || status.code == 404)
}

/// Run one restart-generation check (see [`shadow::check_restart_generation`]),
/// consuming the pending reconnect signal only when the probe *succeeds*: on a
/// probe error `acked_reconnects` stays behind the agent's epoch, so the
/// reconnect remains visible and the check is retried instead of silently
/// dropped.
async fn probe_sozu_generation(
    agent: &SozuAgentHandle,
    acked_reconnects: &mut u64,
    baseline: &mut Option<BTreeSet<i32>>,
    shadow: &mut Ir,
    self_metrics: &metrics::SelfMetrics,
) -> shadow::GenerationCheck {
    // Read the epoch *before* the probe: a reconnect landing mid-probe stays
    // pending and triggers one more (cheap, idempotent) check.
    let pending = agent.reconnect_epoch();
    let outcome = shadow::check_restart_generation(agent, baseline, shadow).await;
    if outcome != shadow::GenerationCheck::ProbeFailed {
        *acked_reconnects = pending;
    }
    if outcome == shadow::GenerationCheck::Reset {
        self_metrics.record_shadow_reset();
    }
    outcome
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
    problem_events: &mut events::ProblemEvents,
    referenced_services: &RwLock<BTreeSet<String>>,
) -> Result<()> {
    let cfg = BuildConfig {
        class_name: args.class_name.clone(),
        class_is_default: class_is_default(stores, &args.class_name),
        controller_name: args.controller_name.clone(),
        http_listener: args.http_listener,
        https_listener: args.https_listener,
        gateway_http_port: args.gateway_http_port,
        gateway_https_port: args.gateway_https_port,
    };
    // The stores hand out `Arc`s to the cached objects; the builder borrows
    // them as-is, so a reconcile never deep-clones the whole cluster state.
    let inputs = Inputs {
        ingresses: stores.ingresses.state(),
        services: stores.services.state(),
        endpointslices: stores.endpointslices.state(),
        secrets: stores.secrets.state(),
        gateway_classes: stores.gateway_classes.state(),
        gateways: stores.gateways.state(),
        http_routes: stores.http_routes.state(),
        reference_grants: stores.reference_grants.state(),
        tcp_services: lookup_configmap(&stores.config_maps, &args.tcp_services_configmap),
        udp_services: lookup_configmap(&stores.config_maps, &args.udp_services_configmap),
    };

    let out = build(&cfg, &inputs);

    // Publish the Services this build referenced (resolved or not) for the
    // EndpointSlice ping filter — before and independent of the apply:
    // relevance follows the *desired* state, not whether the socket push
    // succeeds.
    *referenced_services
        .write()
        .unwrap_or_else(|e| e.into_inner()) = out.referenced_services.clone();

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

    // Surface the problems on their owning objects (kubectl describe), before
    // and independent of the apply: a broken Secret must be visible to its
    // owner even when the socket push fails. Best-effort, diffed against the
    // previous pass so resyncs do not flood etcd with duplicate events.
    problem_events.publish_new(&out).await;

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
        .map(|s| status::gateway_addresses(s))
        .unwrap_or_default();

    // Skippable for least-privilege deployments running without the
    // gateways/status RBAC grants, where every write would 403.
    if args.gateway_status_writes {
        status::write_status(
            client,
            &args.controller_name,
            &out.gateway_classes,
            &out.gateways,
            &out.routes,
            &gw_addresses,
        )
        .await;
    } else {
        debug!("gateway status writes disabled");
    }

    let lb_points = publish_svc
        .map(|s| status::lb_points(s))
        .unwrap_or_default();
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
    // Self-metrics are recorded unconditionally (cheap atomics); the endpoint
    // below only decides whether anyone can scrape them.
    let self_metrics = Arc::new(metrics::SelfMetrics::default());
    if let Some(addr) = args.metrics_listen {
        metrics::spawn(addr, agent.clone(), self_metrics.clone());
    }

    // One signal channel fed by every watcher.
    let (tx, mut rx) = mpsc::channel::<()>(64);

    // `namespace/name` of the Services the last build referenced, shared with
    // the EndpointSlice watcher so endpoint churn from unrelated workloads —
    // the dominant wakeup source on a busy cluster — stops triggering full
    // rebuilds. Empty until the first build; `slice_pings` then passes
    // everything, since a missed wakeup is an outage risk while a spurious
    // one only costs CPU.
    let referenced_services: Arc<RwLock<BTreeSet<String>>> = Arc::new(RwLock::new(BTreeSet::new()));

    let watch_all = watcher::Config::default;
    let (ingresses, w) = reflector::store();
    spawn_watch::<Ingress>(
        Api::all(client.clone()),
        watch_all(),
        w,
        tx.clone(),
        "ingress",
    );
    let (ingress_classes, w) = reflector::store();
    spawn_watch::<IngressClass>(
        Api::all(client.clone()),
        watch_all(),
        w,
        tx.clone(),
        "ingressclass",
    );
    let (services, w) = reflector::store();
    spawn_watch::<Service>(
        Api::all(client.clone()),
        watch_all(),
        w,
        tx.clone(),
        "service",
    );
    let (endpointslices, w) = reflector::store();
    let ping_set = referenced_services.clone();
    spawn_watch_filtered::<EndpointSlice, _>(
        Api::all(client.clone()),
        watch_all(),
        w,
        tx.clone(),
        "endpointslice",
        move |slice| {
            let set = ping_set.read().unwrap_or_else(|e| e.into_inner());
            slice_pings(&set, slice)
        },
    );
    // Only TLS Secrets are of any use to the builder; watching every Secret in
    // the cluster (SA tokens, Helm release blobs, application secrets) would
    // cache them all in this process for nothing — maximal memory cost and
    // maximal blast radius. The field selector bounds both.
    let (secrets, w) = reflector::store();
    spawn_watch::<Secret>(
        Api::all(client.clone()),
        watch_all().fields("type=kubernetes.io/tls"),
        w,
        tx.clone(),
        "secret",
    );

    // ConfigMaps are only watched when L4 (tcp/udp-services) is configured, and
    // then only in the namespaces the specs name (one watcher per distinct
    // namespace): the RBAC grant is namespaced, and a cluster not using L4 pays
    // no ConfigMap-watch cost at all.
    let mut config_maps = Vec::new();
    for ns in l4_namespaces([&args.tcp_services_configmap, &args.udp_services_configmap]) {
        info!(namespace = %ns, "L4 services configured; watching ConfigMaps");
        let (store, w) = reflector::store();
        spawn_watch::<ConfigMap>(
            Api::namespaced(client.clone(), &ns),
            watch_all(),
            w,
            tx.clone(),
            "configmap",
        );
        config_maps.push(store);
    }

    // Gateway API CRDs are optional. Only watch them when they are installed, so
    // an Ingress-only cluster runs cleanly instead of logging watch errors.
    let (gateway_classes, gc_w) = reflector::store();
    let (gateways, gw_w) = reflector::store();
    let (http_routes, hr_w) = reflector::store();
    let (reference_grants, rg_w) = reflector::store();
    let gateway_api_enabled = gateway_api_available(&client).await?;
    if gateway_api_enabled {
        info!("Gateway API detected; watching gateway.networking.k8s.io resources");
        spawn_watch::<GatewayClass>(
            Api::all(client.clone()),
            watch_all(),
            gc_w,
            tx.clone(),
            "gatewayclass",
        );
        spawn_watch::<Gateway>(
            Api::all(client.clone()),
            watch_all(),
            gw_w,
            tx.clone(),
            "gateway",
        );
        spawn_watch::<HttpRoute>(
            Api::all(client.clone()),
            watch_all(),
            hr_w,
            tx.clone(),
            "httproute",
        );
        spawn_watch::<ReferenceGrant>(
            Api::all(client.clone()),
            watch_all(),
            rg_w,
            tx.clone(),
            "referencegrant",
        );
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
    // Every spawned watcher is gated, including the optional ConfigMap and
    // Gateway API ones: a resumed shadow holds Gateway routes and L4 listeners,
    // so reconciling before those caches finish their initial LIST would diff
    // them away (a live-traffic flap). Bounded so a wedged/permission-denied
    // watcher surfaces as a clear failure (CrashLoopBackOff) instead of hanging
    // forever.
    info!("waiting for informer caches to sync...");
    let sync = async {
        tokio::try_join!(
            stores.ingresses.wait_until_ready(),
            stores.ingress_classes.wait_until_ready(),
            stores.services.wait_until_ready(),
            stores.endpointslices.wait_until_ready(),
            stores.secrets.wait_until_ready(),
            // Every per-namespace ConfigMap store in the vec has a watcher.
            futures::future::try_join_all(stores.config_maps.iter().map(|s| s.wait_until_ready())),
            ready_when(gateway_api_enabled, &stores.gateway_classes),
            ready_when(gateway_api_enabled, &stores.gateways),
            ready_when(gateway_api_enabled, &stores.http_routes),
            ready_when(gateway_api_enabled, &stores.reference_grants),
        )
    };
    tokio::time::timeout(Duration::from_secs(120), sync)
        .await
        .context(
            "timed out waiting for informer caches to sync \
             (check RBAC, and that every watched CRD is installed and served)",
        )?
        .context("informer cache writer dropped before becoming ready")?;
    info!("caches synced");

    // Shadow of the last successfully-applied IR. Resumed from the shared volume
    // when Sōzu still holds its state (a controller-only restart), so the first
    // reconcile prunes orphans instead of re-adding everything; otherwise empty,
    // so a fresh/just-restarted Sōzu gets the full state.
    let probe_file = format!("{}.probe", args.shadow_file);
    let mut shadow = shadow::load_initial(&agent, &args.shadow_file, &probe_file).await;

    // Baseline for Sōzu restart detection: its current *worker generation*
    // (live worker-PID set). Any later change — the main process restarting
    // and forking fresh workers, or a single worker bounce — resets the shadow
    // so the full state is re-applied. A failed capture leaves it unset; the
    // first successful check then resets a non-empty shadow (an unproven
    // generation is not trusted) and establishes the baseline.
    let mut worker_baseline = match agent.worker_pids().await {
        Ok(pids) => Some(pids),
        Err(e) => {
            warn!(error = %e, "could not capture Sōzu's worker-PID baseline; will capture it on the first successful probe");
            None
        }
    };
    // The reconnect epoch acknowledged by a successful restart probe; anything
    // newer is a pending reconnect to investigate before trusting the shadow.
    let mut acked_reconnects = agent.reconnect_epoch();

    let debounce = Duration::from_millis(args.debounce_ms);
    let mut resync = resync_period(args.resync_secs).map(resync_interval);
    if resync.is_none() {
        info!("periodic resync disabled (resync-secs = 0)");
    }

    // Graceful shutdown on the signals Kubernetes uses on Pod termination, so we
    // stop cleanly within the grace period instead of being SIGKILLed.
    use tokio::signal::unix::{signal, SignalKind};
    let mut sigterm = signal(SignalKind::terminate()).context("install SIGTERM handler")?;
    let mut sigint = signal(SignalKind::interrupt()).context("install SIGINT handler")?;

    // Event publisher for reported problems, with its diff baseline.
    let mut problem_events = events::ProblemEvents::new(client.clone());

    // Initial reconcile (full apply). Readiness latches once this succeeds.
    let started = std::time::Instant::now();
    match reconcile(
        &args,
        &client,
        &stores,
        &agent,
        &mut shadow,
        &mut problem_events,
        &referenced_services,
    )
    .await
    {
        Ok(()) => {
            self_metrics.record_reconcile(started.elapsed(), true);
            mark_ready(&ready);
        }
        Err(e) => {
            self_metrics.record_reconcile(started.elapsed(), false);
            error!(error = ?e, "initial reconcile failed; will retry");
        }
    }
    // A reconnect can land *during* that first apply (Sōzu restarting
    // mid-batch): probe right away and nudge the loop when a full re-apply or
    // a probe retry is due, instead of waiting for a watch event.
    if agent.reconnect_epoch() != acked_reconnects
        && probe_sozu_generation(
            &agent,
            &mut acked_reconnects,
            &mut worker_baseline,
            &mut shadow,
            &self_metrics,
        )
        .await
            != shadow::GenerationCheck::Unchanged
    {
        let _ = tx.try_send(());
    }

    loop {
        // Wait for a change signal or the resync tick.
        let mut check_sozu = false;
        tokio::select! {
            maybe = rx.recv() => {
                // Defensive only: this loop holds its own tx clone (for
                // self-nudges), so the channel cannot actually close while
                // we are here. If that invariant is ever broken, exit loudly
                // rather than spin on a dead channel.
                if maybe.is_none() { warn!("change channel closed (should be unreachable); exiting"); break; }
                // Debounce: let a burst settle, then drain the queue.
                tokio::time::sleep(debounce).await;
                while rx.try_recv().is_ok() {}
            }
            _ = maybe_tick(resync.as_mut()) => {
                debug!("periodic resync");
                check_sozu = true;
            }
            _ = sigterm.recv() => { info!("SIGTERM received; shutting down"); break; }
            _ = sigint.recv() => { info!("SIGINT received; shutting down"); break; }
        }

        // If Sōzu restarted under us, the agent reconnects transparently and
        // the diff against the stale shadow stays empty — every request would
        // 404 forever. Check Sōzu's restart generation (its worker-PID set) on
        // every resync tick and whenever a reconnect is pending, resetting the
        // shadow on a change so the reconcile below re-applies the full state.
        // The reconnect signal is consumed only by a *successful* probe: on a
        // failure it stays pending and the loop is nudged, so the check is
        // retried promptly even with resync disabled and no watch traffic.
        if agent.reconnect_epoch() != acked_reconnects {
            check_sozu = true;
        }
        if check_sozu
            && probe_sozu_generation(
                &agent,
                &mut acked_reconnects,
                &mut worker_baseline,
                &mut shadow,
                &self_metrics,
            )
            .await
                == shadow::GenerationCheck::ProbeFailed
        {
            let _ = tx.try_send(());
        }

        let started = std::time::Instant::now();
        match reconcile(
            &args,
            &client,
            &stores,
            &agent,
            &mut shadow,
            &mut problem_events,
            &referenced_services,
        )
        .await
        {
            Ok(()) => {
                self_metrics.record_reconcile(started.elapsed(), true);
                mark_ready(&ready);
            }
            Err(e) => {
                self_metrics.record_reconcile(started.elapsed(), false);
                error!(error = ?e, "reconcile failed; will retry on next event/resync");
            }
        }

        // The emptiness race, closed: a reconnect landing *mid-apply* means
        // the rest of the batch was applied to a freshly restarted Sōzu — it
        // is no longer empty, but it only holds that delta. Probe immediately
        // after the apply instead of waiting for the next event; on a reset (a
        // full re-apply is due) or a failed probe (a retry is due), nudge the
        // channel so the next pass runs promptly.
        if agent.reconnect_epoch() != acked_reconnects
            && probe_sozu_generation(
                &agent,
                &mut acked_reconnects,
                &mut worker_baseline,
                &mut shadow,
                &self_metrics,
            )
            .await
                != shadow::GenerationCheck::Unchanged
        {
            let _ = tx.try_send(());
        }
    }

    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    /// An EndpointSlice labelled for `svc` in `ns` (`None` omits the piece).
    fn slice(ns: Option<&str>, svc: Option<&str>) -> EndpointSlice {
        EndpointSlice {
            metadata: k8s_openapi::apimachinery::pkg::apis::meta::v1::ObjectMeta {
                name: Some("slice-1".to_string()),
                namespace: ns.map(str::to_string),
                labels: svc.map(|s| {
                    [("kubernetes.io/service-name".to_string(), s.to_string())]
                        .into_iter()
                        .collect()
                }),
                ..Default::default()
            },
            ..Default::default()
        }
    }

    fn set(keys: &[&str]) -> BTreeSet<String> {
        keys.iter().map(|k| k.to_string()).collect()
    }

    #[test]
    fn slice_pings_only_for_referenced_services() {
        let referenced = set(&["demo/web", "prod/api"]);
        // The slice's service is referenced: ping.
        assert!(slice_pings(&referenced, &slice(Some("demo"), Some("web"))));
        // Same name in another namespace, or another service: no ping — this
        // is exactly the unrelated churn the filter exists to drop.
        assert!(!slice_pings(&referenced, &slice(Some("prod"), Some("web"))));
        assert!(!slice_pings(
            &referenced,
            &slice(Some("demo"), Some("other"))
        ));
        // No service-name label: the builder cannot attribute the slice to
        // any Service, so it can never change the build — no ping.
        assert!(!slice_pings(&referenced, &slice(Some("demo"), None)));
    }

    #[test]
    fn slice_pings_defaults_the_namespace_like_the_builder() {
        // A namespace-less slice must match the builder's `default` fallback,
        // or a referenced default-namespace Service would stop waking us.
        let referenced = set(&["default/web"]);
        assert!(slice_pings(&referenced, &slice(None, Some("web"))));
    }

    #[test]
    fn empty_referenced_set_passes_every_slice() {
        // Before the first build populates the set, a missed wakeup is an
        // outage risk; everything — even unattributable slices — must ping.
        let empty = BTreeSet::new();
        assert!(slice_pings(&empty, &slice(Some("demo"), Some("web"))));
        assert!(slice_pings(&empty, &slice(Some("demo"), None)));
        assert!(slice_pings(&empty, &slice(None, None)));
    }

    #[tokio::test]
    async fn unwatched_store_is_trivially_ready() {
        // A disabled feature (no L4, no Gateway API) drops the writer without
        // spawning a watcher; the sync gate must not wait on (or fail for) it.
        let (store, writer) = reflector::store::<ConfigMap>();
        drop(writer);
        ready_when(false, &store)
            .await
            .expect("an unwatched store must not gate the sync");
    }

    #[tokio::test]
    async fn watched_store_gates_until_its_initial_list_lands() {
        let (store, mut writer) = reflector::store::<ConfigMap>();
        // Before the initial LIST completes, the gate must still be waiting —
        // this is exactly the window where reconciling would flap live routes.
        let waiting =
            tokio::time::timeout(Duration::from_millis(50), ready_when(true, &store)).await;
        assert!(waiting.is_err(), "gate must hold until the cache syncs");

        writer.apply_watcher_event(&watcher::Event::Init);
        writer.apply_watcher_event(&watcher::Event::InitDone);
        ready_when(true, &store)
            .await
            .expect("gate must open once the initial LIST is applied");
    }

    #[tokio::test]
    async fn watched_store_with_dropped_writer_fails_fast() {
        // If a watched store's writer is gone the gate must error (fail fast),
        // never report ready.
        let (store, writer) = reflector::store::<ConfigMap>();
        drop(writer);
        assert!(ready_when(true, &store).await.is_err());
    }

    #[test]
    fn only_a_404_reads_as_crds_absent() {
        use kube::core::Status;

        // What the apiserver returns when the CRD's group/kind is not served.
        let not_found = kube::Error::Api(
            Status::failure(
                "the server could not find the requested resource",
                "NotFound",
            )
            .with_code(404)
            .boxed(),
        );
        assert!(gateway_crds_absent(&not_found));

        // Managed clusters fronting the apiserver with an HTTP router answer
        // an unserved group's path with a plain-text `404 page not found`;
        // kube-client can't parse that body into a typed Status and
        // synthesizes this parse-failure reason. Still a 404 on a collection
        // list, so still "absent" — matching on the reason alone crash-looped
        // the controller on such clusters.
        let unparsed_404 = kube::Error::Api(
            Status::failure("404 page not found\n", "Failed to parse error data")
                .with_code(404)
                .boxed(),
        );
        assert!(gateway_crds_absent(&unparsed_404));

        // RBAC not yet propagated: a transient failure, never "absent".
        let forbidden = kube::Error::Api(
            Status::failure("gatewayclasses is forbidden", "Forbidden")
                .with_code(403)
                .boxed(),
        );
        assert!(!gateway_crds_absent(&forbidden));

        // An apiserver hiccup must fail fast, not lock in Ingress-only mode.
        let unavailable = kube::Error::Api(
            Status::failure("etcdserver: request timed out", "InternalError")
                .with_code(500)
                .boxed(),
        );
        assert!(!gateway_crds_absent(&unavailable));
    }

    #[test]
    fn any_missing_crd_forces_ingress_only_mode() {
        // Full install: gateway mode.
        assert!(missing_gateway_crds([true, true, true, true]).is_empty());
        // Partial install (e.g. the standard channel applied without
        // ReferenceGrant): Ingress-only, naming exactly the missing kind —
        // gateway mode would watch it, never sync, and crash-loop at the
        // cache gate.
        assert_eq!(
            missing_gateway_crds([true, true, true, false]),
            vec!["ReferenceGrant"]
        );
        assert_eq!(
            missing_gateway_crds([true, false, true, false]),
            vec!["Gateway", "ReferenceGrant"]
        );
        // Nothing installed: the ordinary Ingress-only cluster.
        assert_eq!(
            missing_gateway_crds([false, false, false, false]),
            GATEWAY_API_KINDS.to_vec()
        );
    }

    fn cm(ns: &str, name: &str) -> ConfigMap {
        ConfigMap {
            metadata: kube::api::ObjectMeta {
                namespace: Some(ns.to_string()),
                name: Some(name.to_string()),
                ..Default::default()
            },
            ..Default::default()
        }
    }

    fn synced_store(objects: Vec<ConfigMap>) -> Store<ConfigMap> {
        let (store, mut writer) = reflector::store();
        writer.apply_watcher_event(&watcher::Event::Init);
        for obj in objects {
            writer.apply_watcher_event(&watcher::Event::InitApply(obj));
        }
        writer.apply_watcher_event(&watcher::Event::InitDone);
        store
    }

    #[test]
    fn l4_watch_covers_exactly_the_namespaces_the_specs_name() {
        // The chart's common case: both maps in the release namespace — one
        // watcher, not one per map.
        assert_eq!(
            l4_namespaces([&Some("gw/tcp".to_string()), &Some("gw/udp".to_string())]),
            BTreeSet::from(["gw".to_string()])
        );
        // The two specs may name different namespaces: one watcher each.
        assert_eq!(
            l4_namespaces([&Some("a/tcp".to_string()), &Some("b/udp".to_string())]),
            BTreeSet::from(["a".to_string(), "b".to_string()])
        );
        // Unset or malformed (no `/`) specs must not spawn a watcher: the
        // lookup could never resolve them, and RBAC only covers named
        // namespaces.
        assert!(l4_namespaces([&None, &Some("no-slash".to_string())]).is_empty());
    }

    #[test]
    fn lookup_configmap_searches_every_namespace_store() {
        // Two L4 specs in different namespaces mean two per-namespace caches;
        // the lookup must find a map wherever it lives, and still match on
        // the full namespace/name (never on name alone).
        let stores = vec![
            synced_store(vec![cm("gw", "tcp-services")]),
            synced_store(vec![cm("other", "udp-services")]),
        ];
        let spec = |s: &str| Some(s.to_string());
        assert!(lookup_configmap(&stores, &spec("gw/tcp-services")).is_some());
        assert!(lookup_configmap(&stores, &spec("other/udp-services")).is_some());
        assert!(lookup_configmap(&stores, &spec("other/tcp-services")).is_none());
        assert!(lookup_configmap(&stores, &None).is_none());
    }

    #[test]
    fn gateway_status_writes_default_on_and_are_disablable() {
        // Conditions are the Gateway API's UX: the default must stay on. The
        // explicit off-switch exists for deployments without the */status
        // RBAC grants.
        let args = Args::parse_from(["sozu-gw-controller"]);
        assert!(args.gateway_status_writes);
        let args = Args::parse_from(["sozu-gw-controller", "--gateway-status-writes", "false"]);
        assert!(!args.gateway_status_writes);
    }

    #[test]
    fn zero_resync_secs_disables_the_periodic_resync() {
        // 0 must read as "disabled", never reach tokio's zero-interval panic.
        assert_eq!(resync_period(0), None);
        assert_eq!(resync_period(60), Some(Duration::from_secs(60)));
    }

    #[tokio::test]
    async fn disabled_resync_arm_never_fires() {
        let fired = tokio::time::timeout(Duration::from_millis(50), maybe_tick(None)).await;
        assert!(fired.is_err(), "a disabled resync must pend forever");
    }

    #[tokio::test(start_paused = true)]
    async fn resync_interval_first_tick_lands_one_period_after_startup() {
        let mut interval = resync_interval(Duration::from_secs(60));
        // A raw `interval()` would tick immediately, re-running a redundant
        // reconcile right after the initial one.
        let early =
            tokio::time::timeout(Duration::from_secs(1), maybe_tick(Some(&mut interval))).await;
        assert!(early.is_err(), "first tick must not complete immediately");
        // ... but it must still fire once a full period has elapsed.
        let due =
            tokio::time::timeout(Duration::from_secs(120), maybe_tick(Some(&mut interval))).await;
        assert!(due.is_ok(), "the interval must tick after one period");
    }
}
