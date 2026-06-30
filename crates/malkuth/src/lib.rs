//! # malkuth
//!
//! Runtime-agnostic service-supervision toolkit: JSON-RPC over pluggable
//! transports (TCP / WebSocket / IPC), supervised workers, split health probes
//! and coordination locks. Built on [`malkuth_core`] and the `futures_io`
//! async-I/O family, so it runs under tokio, async-std or smol.

pub mod client;
pub mod codec;
pub mod jsonrpc;
pub mod server;
pub mod service;
pub mod transport;

#[cfg(feature = "worker")]
pub mod worker;
#[cfg(feature = "axum-probe")]
pub mod probes;
#[cfg(feature = "signals")]
pub mod signals;
#[cfg(feature = "replica")]
pub mod registry;
#[cfg(feature = "pg-lock")]
pub mod pg_lock;
#[cfg(feature = "leader-follower")]
pub mod leader;

pub use client::Client;
pub use jsonrpc::{Id, Request, Response, RpcError, RpcHandler, Router};
pub use server::Server;
pub use service::Supervised;
#[cfg(feature = "leader-follower")]
pub use leader::LeaseLeaderElector;
#[cfg(feature = "pg-lock")]
pub use pg_lock::PgLock;

pub use malkuth_core;
