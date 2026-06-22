//! Intermediate Representation (IR) for the Sōzu gateway controller.
//!
//! Neutral, I/O-free Rust structures mapped 1:1 onto Sōzu's routing vocabulary.
//! The Builder produces this from Kubernetes objects; the Translator consumes it
//! to emit Sōzu protobuf commands. This crate depends on neither `kube` nor the
//! command socket, so it is unit-testable in isolation.
//!
//! Listeners are intentionally **not** modelled here: in Phase 1 they are
//! declared statically in Sōzu's `config.toml` and activated at boot, so the
//! controller only manages clusters / frontends / backends / certificates.
#![forbid(unsafe_code)]

use std::net::SocketAddr;

use serde::{Deserialize, Serialize};

/// Load-balancing algorithm for a cluster (the subset meaningful in Phase 1).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default, Serialize, Deserialize)]
pub enum LbAlgorithm {
    #[default]
    RoundRobin,
    Random,
    LeastLoaded,
    PowerOfTwo,
}

/// How an Ingress path is matched, mapped from Kubernetes `pathType`.
#[derive(Debug, Clone, PartialEq, Eq, PartialOrd, Ord, Serialize, Deserialize)]
pub enum PathMatch {
    /// `pathType: Prefix`
    Prefix(String),
    /// `pathType: Exact`
    Exact(String),
    /// `pathType: ImplementationSpecific` → regex
    Regex(String),
}

/// A routing target: one Sōzu cluster, typically one per Service:port.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Cluster {
    pub id: String,
    pub load_balancing: LbAlgorithm,
    pub sticky_session: bool,
    pub https_redirect: bool,
}

/// One backend endpoint: a **pod IP:port** (never a ClusterIP).
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Backend {
    pub cluster_id: String,
    /// Stable id per endpoint so add/remove are idempotent across resyncs.
    pub backend_id: String,
    pub address: SocketAddr,
    /// Optional weight; `None` means equal weighting (Sōzu default).
    pub weight: Option<i32>,
}

/// A route: hostname + path (+ method) → cluster, on the HTTP or HTTPS listener.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Frontend {
    pub hostname: String,
    pub path: PathMatch,
    pub method: Option<String>,
    pub cluster_id: String,
    /// `true` => HTTPS listener (`AddHttpsFrontend`), `false` => HTTP.
    pub tls: bool,
    /// The listener address this frontend attaches to (e.g. `0.0.0.0:80`).
    pub listener: SocketAddr,
}

/// A TLS certificate loaded onto the HTTPS listener (from a K8s TLS Secret).
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Certificate {
    /// The HTTPS listener address the cert is bound to (e.g. `0.0.0.0:443`).
    pub listener: SocketAddr,
    /// Leaf certificate, PEM.
    pub certificate: String,
    /// Intermediate chain, PEM (empty for self-signed).
    pub chain: Vec<String>,
    /// Private key, PEM.
    pub key: String,
    /// SNI names to serve this cert for (the Ingress TLS hosts).
    pub names: Vec<String>,
}

/// The complete desired routing state compiled from all our Ingress objects.
#[derive(Debug, Clone, PartialEq, Eq, Default, Serialize, Deserialize)]
pub struct Ir {
    pub clusters: Vec<Cluster>,
    pub frontends: Vec<Frontend>,
    pub backends: Vec<Backend>,
    pub certificates: Vec<Certificate>,
}
