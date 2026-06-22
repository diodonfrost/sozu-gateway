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
use std::time::Duration;

use anyhow::{Context, Result};
use clap::Parser;
use futures::StreamExt;
use k8s_openapi::api::core::v1::{Secret, Service};
use k8s_openapi::api::discovery::v1::EndpointSlice;
use k8s_openapi::api::networking::v1::{Ingress, IngressClass};
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
use sozu_gw_translator as tr;

const DEFAULT_CLASS_ANNOTATION: &str = "ingressclass.kubernetes.io/is-default-class";

#[derive(Parser, Debug, Clone)]
#[command(name = "sozu-gw-controller", about = "Sōzu-based Ingress controller")]
struct Args {
    /// IngressClass name we own.
    #[arg(long, env = "SOZU_GW_CLASS", default_value = "sozu")]
    class_name: String,
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
}

/// Reflector read handles for every watched resource type.
struct Stores {
    ingresses: Store<Ingress>,
    ingress_classes: Store<IngressClass>,
    services: Store<Service>,
    endpointslices: Store<EndpointSlice>,
    secrets: Store<Secret>,
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
                    warn!(watch = kind, "watch stream ended");
                    break;
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

/// One global reconcile: caches → IR → diff → apply. Updates `shadow` (the
/// last-applied IR) only on a successful apply, so a failed push is retried from
/// the same baseline.
async fn reconcile(
    args: &Args,
    stores: &Stores,
    agent: &SozuAgentHandle,
    shadow: &mut Ir,
) -> Result<()> {
    let cfg = BuildConfig {
        class_name: args.class_name.clone(),
        class_is_default: class_is_default(stores, &args.class_name),
        http_listener: args.http_listener,
        https_listener: args.https_listener,
    };
    let inputs = Inputs {
        ingresses: collect(&stores.ingresses),
        services: collect(&stores.services),
        endpointslices: collect(&stores.endpointslices),
        secrets: collect(&stores.secrets),
    };

    let out = build(&cfg, &inputs);
    for r in &out.results {
        if !r.problems.is_empty() {
            warn!(namespace = %r.namespace, name = %r.name, problems = ?r.problems, "ingress has problems");
        }
    }

    let requests = tr::reconcile(shadow, &out.ir).context("translate IR to commands")?;
    if requests.is_empty() {
        debug!("reconcile: no changes");
        return Ok(());
    }

    info!(
        clusters = out.ir.clusters.len(),
        backends = out.ir.backends.len(),
        frontends = out.ir.frontends.len(),
        certificates = out.ir.certificates.len(),
        requests = requests.len(),
        "applying changes to sozu"
    );
    agent
        .apply(requests)
        .await
        .context("apply requests to sozu")?;
    *shadow = out.ir;
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

    let client = Client::try_default()
        .await
        .context("create kube client (in-cluster or kubeconfig)")?;
    let agent = SozuAgentHandle::spawn(&args.socket).context("spawn sozu-agent")?;

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

    let stores = Stores {
        ingresses,
        ingress_classes,
        services,
        endpointslices,
        secrets,
    };

    // Wait for the caches to fill so the first reconcile sees a complete picture.
    info!("waiting for informer caches to sync...");
    stores.ingresses.wait_until_ready().await?;
    stores.ingress_classes.wait_until_ready().await?;
    stores.services.wait_until_ready().await?;
    stores.endpointslices.wait_until_ready().await?;
    stores.secrets.wait_until_ready().await?;
    info!("caches synced");

    // Shadow of the last successfully-applied IR. Starts empty: the first
    // reconcile diffs empty→desired, i.e. pushes the full state at startup.
    let mut shadow = Ir::default();

    let debounce = Duration::from_millis(args.debounce_ms);
    let mut resync = tokio::time::interval(Duration::from_secs(args.resync_secs));
    resync.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Skip);

    // Initial reconcile (full apply).
    if let Err(e) = reconcile(&args, &stores, &agent, &mut shadow).await {
        error!(error = ?e, "initial reconcile failed; will retry");
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
        }

        if let Err(e) = reconcile(&args, &stores, &agent, &mut shadow).await {
            error!(error = ?e, "reconcile failed; will retry on next event/resync");
        }
    }

    Ok(())
}
