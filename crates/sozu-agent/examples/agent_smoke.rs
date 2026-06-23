//! End-to-end smoke test of the pure pipeline against a live Sōzu (Étape 3).
//!
//! Builds an IR, runs it through the Translator, and applies the resulting
//! requests via [`SozuAgentHandle`] — exercising IR → Translator → Agent →
//! socket. The surrounding harness then proves traffic with curl/openssl.
//!
//! It applies the full state twice to confirm idempotency at the diff layer:
//! the second `diff(shadow, desired)` is empty, so no commands are sent.
//!
//! Env vars mirror `examples/probe.rs` (SOZU_SOCK, PROBE_HTTP/HTTPS/BACKEND/HOST,
//! PROBE_CERT, PROBE_KEY).

use std::net::SocketAddr;

use anyhow::{Context, Result};
use sozu_gw_agent::SozuAgentHandle;
use sozu_gw_ir as ir;
use sozu_gw_translator as tr;

fn env_or(key: &str, default: &str) -> String {
    std::env::var(key).unwrap_or_else(|_| default.to_string())
}

fn addr(s: &str) -> Result<SocketAddr> {
    s.parse().with_context(|| format!("invalid addr {s:?}"))
}

#[tokio::main]
async fn main() -> Result<()> {
    let sock = env_or("SOZU_SOCK", "./sozu.sock");
    let listen_http = addr(&env_or("PROBE_HTTP", "0.0.0.0:8080"))?;
    let listen_https = addr(&env_or("PROBE_HTTPS", "0.0.0.0:8443"))?;
    let backend = addr(&env_or("PROBE_BACKEND", "127.0.0.1:9000"))?;
    let host = env_or("PROBE_HOST", "app.example.com");
    let cert = std::fs::read_to_string(env_or("PROBE_CERT", "./app.crt"))?;
    let key = std::fs::read_to_string(env_or("PROBE_KEY", "./app.key"))?;
    let cluster_id = host.replace('.', "-");

    let model = ir::Ir {
        clusters: vec![ir::Cluster {
            id: cluster_id.clone(),
            load_balancing: ir::LbAlgorithm::RoundRobin,
            sticky_session: false,
            https_redirect: false,
        }],
        backends: vec![ir::Backend {
            cluster_id: cluster_id.clone(),
            backend_id: format!("{cluster_id}-0"),
            address: backend,
            weight: None,
        }],
        frontends: vec![
            ir::Frontend {
                hostname: host.clone(),
                path: ir::PathMatch::Prefix("/".into()),
                method: None,
                cluster_id: Some(cluster_id.clone()),
                tls: false,
                listener: listen_http,
                filters: ir::FrontendFilters::default(),
            },
            ir::Frontend {
                hostname: host.clone(),
                path: ir::PathMatch::Prefix("/".into()),
                method: None,
                cluster_id: Some(cluster_id.clone()),
                tls: true,
                listener: listen_https,
                filters: ir::FrontendFilters::default(),
            },
        ],
        certificates: vec![ir::Certificate {
            listener: listen_https,
            certificate: cert,
            chain: vec![],
            key,
            names: vec![host.clone()],
        }],
    };

    let handle = SozuAgentHandle::spawn(&sock).context("spawn agent")?;

    // First reconcile: shadow is empty -> apply the full desired state.
    let requests = tr::reconcile(&ir::Ir::default(), &model)?;
    println!("[agent_smoke] applying {} requests", requests.len());
    handle.apply(requests).await.context("apply full state")?;
    println!("[agent_smoke] full state applied OK");

    // Second reconcile from the updated shadow: must be a no-op (idempotent).
    let again = tr::reconcile(&model, &model)?;
    println!(
        "[agent_smoke] second diff has {} requests (expect 0)",
        again.len()
    );
    anyhow::ensure!(
        again.is_empty(),
        "idempotency broken: re-diff was non-empty"
    );
    handle.apply(again).await.context("apply (empty)")?;

    println!("[agent_smoke] DONE — run curl/openssl checks now.");
    Ok(())
}
