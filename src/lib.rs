//! # plana
//!
//! A small, generic Rust toolkit for supervising long-running services:
//!
//! - uniform signal/drain semantics (`SIGTERM`/`SIGINT` = drain, `SIGHUP` =
//!   reload, `SIGQUIT` = immediate),
//! - split `/healthz` (liveness) + `/readyz` (readiness, with a drain bit)
//!   probes,
//! - supervised child-process workers with OTP-style restart policy and
//!   sliding-window rate limiting,
//! - systemd socket-activation listener handoff (zero-downtime restart),
//! - a pluggable coordination-lock abstraction (file / pg / lease).
//!
//! These are the building blocks for load-balanced replicas, rolling
//! updates and leader/follower HA. Originally factored out of the
//! celestia-island platform (entelecheia, shittim-chest, evernight); plana
//! itself is framework-light and depends only on tokio + axum.

pub mod lifecycle;
pub mod listener;
pub mod lock;
pub mod probes;
pub mod types;
pub mod worker;

#[cfg(feature = "leader-follower")]
pub mod leader;
#[cfg(feature = "replica")]
pub mod replica;

pub use lifecycle::{DrainController, ShutdownKind};
pub use listener::acquire_listener;
pub use lock::{CoordinationLock, LockError, LockGuard};
pub use probes::{ProbeState, probe_router};
pub use types::*;
pub use worker::{Supervisor, WorkerSpec};
