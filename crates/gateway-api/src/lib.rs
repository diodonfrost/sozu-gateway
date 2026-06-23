//! Gateway API CRD types (`gateway.networking.k8s.io`).
//!
//! Generated from the upstream **v1.2.1** standard-channel CRDs with `kopium`
//! (regeneration steps in [README.md](README.md)). Types only: `kube` is pulled
//! for the `CustomResource` derive (which provides the `kube::Resource` impl the
//! watchers need), not for any client or runtime I/O.
//!
//! The modules are generated code; lints are relaxed on them so the rest of the
//! workspace can keep `clippy -D warnings`.
#![forbid(unsafe_code)]

#[allow(clippy::all, non_snake_case)]
pub mod gateway;
#[allow(clippy::all, non_snake_case)]
pub mod gatewayclass;
#[allow(clippy::all, non_snake_case)]
pub mod httproute;
#[allow(clippy::all, non_snake_case)]
pub mod referencegrant;

pub use gateway::{Gateway, GatewaySpec, GatewayStatus};
pub use gatewayclass::{GatewayClass, GatewayClassSpec, GatewayClassStatus};
pub use httproute::{HttpRoute, HttpRouteSpec, HttpRouteStatus};
pub use referencegrant::{ReferenceGrant, ReferenceGrantSpec};
