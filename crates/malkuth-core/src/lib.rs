//! # malkuth-core
//!
//! The **contract layer** of malkuth: wire/protocol types and trait
//! definitions that are independent of any async runtime (tokio / async-std /
//! smol) and any server framework (axum / hyper / …).
//!
//! Everything here is deliberately free of:
//! - a specific async runtime (only [`futures_io`] / [`event_listener`] are
//!   used, which work under *any* executor), and
//! - a specific wire framework (HTTP, axum, etc. are implementation choices
//!   made one layer up in the `malkuth` crate).
//!
//! Concrete runtime/framework implementations live in the `malkuth` crate;
//! this crate is what they all agree on.

pub mod hooks;
pub mod lifecycle;
pub mod transport;
pub mod traits;
pub mod types;

#[cfg(all(unix, feature = "file-lock"))]
pub mod lock;

pub use hooks::{
    DrainHook, ExitReason, ExitSource, Heartbeat, HeartbeatReport, ProbeSink,
};
pub use lifecycle::{DrainController, ShutdownKind};
pub use transport::{FramedConn, Transport, WireConn, WireListener, take_frame};
pub use traits::{CoordinationLock, InstanceRegistry, LeaderElector, LockError, LockGuard};
pub use types::*;
