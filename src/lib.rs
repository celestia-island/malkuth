//! # malkuth
//!
//! Service-supervision toolkit for long-running Rust programs: JSON-RPC over
//! pluggable transports (TCP / WebSocket / IPC), supervised workers, split
//! health probes, coordination locks and lease-based leader election.
//!
//! ## Feature flags
//!
//! | Feature | What it enables |
//! | --- | --- |
//! | `tcp` *(default)* | TCP JSON-RPC transport |
//! | `ws` | WebSocket transport |
//! | `ipc` | Unix-domain-socket / named-pipe transport |
//! | `signals` *(default)* | OS-signal exit source (SIGINT/TERM/HUP/QUIT) |
//! | `worker` | OTP-style child-process supervision |
//! | `probes` | axum `/healthz` + `/readyz` routes |
//! | `file-lock` | POSIX `flock` coordination-lock backend (Unix only) |
//! | `lease` | File-lease lock with TTL auto-expiry |
//! | `pg-lock` | PostgreSQL advisory-lock backend |
//! | `replica` | In-memory instance registry |
//! | `leader-follower` | Lease-based leader elector |
//! | `cli` | `malkuth` watchdog binary (pod pool + sticky proxy) |

// ── Layer 1: lifecycle & wire types ────────────────────────────
pub mod hooks;
pub mod lifecycle;
pub mod traits;
pub mod types;
pub mod wire;

// ── Layer 2: coordination backends ─────────────────────────────
#[cfg(feature = "lease")]
pub mod lease;
#[cfg(all(unix, feature = "file-lock"))]
pub mod lock;

// ── Layer 3: JSON-RPC + transports ─────────────────────────────
pub mod client;
pub mod codec;
pub mod jsonrpc;
pub mod server;
pub mod service;

#[cfg(any(feature = "tcp", feature = "ws", feature = "ipc"))]
pub mod transport;

// ── Layer 4: runtime facilities ────────────────────────────────
#[cfg(feature = "leader-follower")]
pub mod leader;
#[cfg(feature = "pg-lock")]
pub mod pg_lock;
#[cfg(feature = "probes")]
pub mod probes;
#[cfg(feature = "replica")]
pub mod registry;
#[cfg(feature = "signals")]
pub mod signals;
#[cfg(feature = "worker")]
pub mod worker;

// ── Convenience re-exports ─────────────────────────────────────
pub use hooks::{DrainHook, ExitReason, ExitSource, Heartbeat, HeartbeatReport, ProbeSink};
pub use jsonrpc::{Id, Request, Response, Router, RpcError, RpcHandler};
pub use lifecycle::{DrainController, ShutdownKind};
pub use traits::{CoordinationLock, InstanceRegistry, LeaderElector, LockError, LockGuard};
pub use types::*;
pub use wire::{Transport, WireConn, WireListener};

pub use client::{Client, ClientPool};
pub use server::Server;
pub use service::Supervised;

#[cfg(feature = "worker")]
pub use worker::{Supervisor, WorkerSpec};

#[cfg(feature = "probes")]
pub use probes::{ProbeState, probe_router};
