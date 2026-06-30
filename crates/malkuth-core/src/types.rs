//! Lifecycle, supervision & rolling-update wire types.
//!
//! These are the protocol types that cross process boundaries (JSON-RPC over
//! loopback / WebSocket / IPC, HTTP probes, instance-registry queries). The
//! matching runtime behaviour lives in the `malkuth` crate.

use serde::{Deserialize, Serialize};

#[cfg(feature = "schema")]
use schemars::JsonSchema;

// Re-export a schema-conditional derive helper so every type below can be
// written once and gain `JsonSchema` only when the `schema` feature is on.
#[cfg(feature = "schema")]
macro_rules! wire {
    ($($t:tt)*) => {
        #[derive(Debug, Clone, JsonSchema, Serialize, Deserialize)]
        $($t)*
    };
}
#[cfg(not(feature = "schema"))]
macro_rules! wire {
    ($($t:tt)*) => {
        #[derive(Debug, Clone, Serialize, Deserialize)]
        $($t)*
    };
}

// ═══════════════════════════════════════════════════════════════
// JSON-RPC method names
// ═══════════════════════════════════════════════════════════════

/// JSON-RPC method names used by the lifecycle / supervision protocol.
///
/// Kept as a `pub const` module (rather than an enum) so they can be passed
/// directly as `&str` wire identifiers without extra ceremony.
pub mod methods {
    /// Ask an instance to enter drain (graceful shutdown). Server-bound.
    pub const DRAIN: &str = "Lifecycle.Drain";
    /// Ask an instance to hot-reload its configuration. Server-bound.
    pub const RELOAD: &str = "Lifecycle.Reload";
    /// Query an instance's lifecycle status (drain state + readiness).
    pub const STATUS: &str = "Lifecycle.Status";
    /// Query liveness (the body of `GET /healthz`, exposed over RPC too).
    pub const HEALTH: &str = "Lifecycle.Health";
    /// Report a worker's status to its supervisor. Worker→supervisor.
    pub const WORKER_STATUS: &str = "Worker.Status";
    /// Register/update an instance in the shared instance registry.
    pub const INSTANCE_REGISTER: &str = "Lifecycle.InstanceRegister";
    /// Deregister an instance that has exited.
    pub const INSTANCE_DEREGISTER: &str = "Lifecycle.InstanceDeregister";
    /// List instances known to the registry (for LB / orchestrator).
    pub const INSTANCE_LIST: &str = "Lifecycle.InstanceList";
    /// Leader/follower (Subsystem B): announce a lease acquisition/transfer.
    pub const LEADER_ANNOUNCE: &str = "Lifecycle.LeaderAnnounce";
    /// Heartbeat: a worker/instance periodically pushes a liveness beat.
    pub const HEARTBEAT: &str = "Lifecycle.Heartbeat";
}

// ═══════════════════════════════════════════════════════════════
// Drain state
// ═══════════════════════════════════════════════════════════════

/// High-level lifecycle state of an instance, observable over the wire.
wire! {
    #[serde(rename_all = "snake_case")]
    #[derive(PartialEq, Eq, Copy, Default)]
    pub enum DrainState {
        /// Serving normally; accepts new work.
        #[default]
        Active,
        /// Graceful shutdown in progress; refuses new work, finishes in-flight.
        Draining,
        /// Hot configuration reload in progress (SIGHUP); transient.
        Reloading,
    }
}

// ═══════════════════════════════════════════════════════════════
// Health probes
// ═══════════════════════════════════════════════════════════════

/// Result of one dependency check reported by `/readyz`.
wire! {
    pub struct DependencyCheck {
        /// Human-readable dependency name, e.g. `database`, `scepter_socket`.
        pub name: String,
        /// Whether the dependency is currently healthy.
        pub ok: bool,
        /// Optional detail / error message when `ok` is false.
        #[serde(default, skip_serializing_if = "Option::is_none")]
        pub detail: Option<String>,
    }
}

/// Readiness probe payload — the body of `GET /readyz`.
///
/// The `draining` flag is the central rolling-update signal: a load balancer
/// or orchestrator routes new traffic only to instances whose `ready && !draining`.
wire! {
    pub struct ReadyStatus {
        /// Overall readiness: true only when not draining AND every dependency ok.
        pub ready: bool,
        /// True while the instance is draining (graceful shutdown) or reloading.
        pub draining: bool,
        /// Per-dependency checks that contributed to `ready`.
        #[serde(default)]
        pub dependencies: Vec<DependencyCheck>,
        /// Deployment generation this instance belongs to (rolling-update bookkeeping).
        #[serde(default, skip_serializing_if = "Option::is_none")]
        pub generation: Option<u64>,
    }
}

/// Liveness probe payload — the body of `GET /healthz`.
///
/// Intentionally minimal: if the process can answer at all, it is alive.
wire! {
    pub struct HealthStatus {
        /// Always true when the endpoint answers (else the probe fails).
        pub alive: bool,
        /// OS process id of the server.
        pub pid: u32,
        /// Seconds since the instance started.
        pub uptime_secs: u64,
        /// Build version string.
        pub version: String,
    }
}

// ═══════════════════════════════════════════════════════════════
// Instance registry (Layer 2 — used mainly during the upgrade window)
// ═══════════════════════════════════════════════════════════════

/// Role of an instance within its group.
wire! {
    #[serde(rename_all = "snake_case")]
    #[derive(PartialEq, Eq, Copy)]
    pub enum InstanceRole {
        /// Accepting new work.
        Active,
        /// Retiring; refuses new work, finishing in-flight.
        Draining,
    }
}

/// One entry in the shared instance registry.
wire! {
    pub struct InstanceInfo {
        /// Stable unique id of this instance (uuid recommended).
        pub instance_id: String,
        /// Logical group this instance belongs to (e.g. service name).
        pub group: String,
        /// Current role.
        pub role: InstanceRole,
        /// Deployment generation (incremented on each rolling update).
        pub generation: u64,
        /// ISO-8601 timestamp at which the instance started.
        pub started_at: String,
        /// Optional endpoint (host:port or socket path) clients can reach it at.
        #[serde(default, skip_serializing_if = "Option::is_none")]
        pub endpoint: Option<String>,
        /// Optional backend address (host:port) a reverse proxy should forward to.
        #[serde(default, skip_serializing_if = "Option::is_none")]
        pub backend: Option<String>,
    }
}

// ═══════════════════════════════════════════════════════════════
// Leader / follower (Subsystem B)
// ═══════════════════════════════════════════════════════════════

/// Leader/follower role for Subsystem B (active-passive HA).
wire! {
    #[serde(rename_all = "snake_case")]
    #[derive(PartialEq, Eq, Copy)]
    pub enum LeaderRole {
        /// Currently holds the lease and owns the exclusive resources.
        Leader,
        /// Standing by; will promote if the leader's lease expires.
        Follower,
    }
}

/// Announcement of a lease acquisition or transfer (method `LEADER_ANNOUNCE`).
wire! {
    pub struct LeaderAnnounce {
        /// Group/device identity this leadership applies to (shared by leader & follower).
        pub node_id: String,
        /// Id of the instance now holding the lease.
        pub leader_instance_id: String,
        /// Monotonic term/generation of this leadership, to reject stale announcements.
        pub term: u64,
        /// ISO-8601 timestamp at which the lease was acquired.
        pub acquired_at: String,
        /// Lease time-to-live in seconds; followers promote only after it elapses.
        pub lease_ttl_secs: u32,
    }
}

// ═══════════════════════════════════════════════════════════════
// Worker supervision
// ═══════════════════════════════════════════════════════════════

/// Lifecycle state of one supervised worker process (the FSM in the design).
wire! {
    #[serde(rename_all = "snake_case")]
    #[derive(PartialEq, Eq, Copy)]
    pub enum WorkerStatus {
        /// Process spawned, not yet confirmed healthy.
        Starting,
        /// Running and healthy.
        Running,
        /// Stopped (intentional or after cooldown).
        Stopped,
        /// Crashed / unhealthy and pending restart (subject to policy + rate limit).
        Failed,
    }
}

/// When to restart a worker after it exits (OTP vocabulary).
wire! {
    #[serde(rename_all = "snake_case")]
    #[derive(PartialEq, Eq, Copy, Default)]
    pub enum RestartPolicy {
        /// Always restart, even on clean exit (default for resource workers).
        #[default]
        Permanent,
        /// Restart only on abnormal (non-zero) exit.
        Transient,
        /// Never restart.
        Temporary,
    }
}

/// Snapshot of one worker, reported over `Worker.Status`.
wire! {
    pub struct WorkerInfo {
        /// Worker identifier (unique within its supervisor).
        pub id: String,
        /// Resource kind this worker holds (e.g. `modbus`, `s7comm`, `cosmos`).
        pub kind: String,
        /// Current lifecycle state.
        pub status: WorkerStatus,
        /// Configured restart policy.
        pub restart_policy: RestartPolicy,
        /// Number of (re)starts observed by the supervisor.
        #[serde(default)]
        pub restart_count: u32,
        /// Last error detail, if the worker is `Failed`.
        #[serde(default, skip_serializing_if = "Option::is_none")]
        pub last_error: Option<String>,
    }
}

// ═══════════════════════════════════════════════════════════════
// RPC request/response bodies
// ═══════════════════════════════════════════════════════════════

/// Body of `Lifecycle.Drain` — ask an instance to enter graceful shutdown.
wire! {
    pub struct DrainRequest {
        /// Override the default drain timeout (seconds). `None` = use instance default.
        #[serde(default, skip_serializing_if = "Option::is_none")]
        pub timeout_secs: Option<u32>,
        /// Free-form reason (e.g. `rolling_update`, `manual`, `leader_demote`).
        #[serde(default, skip_serializing_if = "Option::is_none")]
        pub reason: Option<String>,
    }
}

/// Reply to `Lifecycle.Drain`.
wire! {
    pub struct DrainResponse {
        /// Whether the instance accepted the drain request.
        pub accepted: bool,
        /// Whether it is now draining (true once accepted).
        pub draining: bool,
    }
}

/// Body of `Lifecycle.Heartbeat` — a periodic liveness beat pushed by a
/// worker/instance to its supervisor / registry.
wire! {
    pub struct HeartbeatBeat {
        /// Id of the instance emitting the beat.
        pub instance_id: String,
        /// Group the instance belongs to.
        pub group: String,
        /// ISO-8601 timestamp of the beat.
        pub ts: String,
        /// Optional backend address the instance is currently serving on.
        #[serde(default, skip_serializing_if = "Option::is_none")]
        pub backend: Option<String>,
        /// Optional generation.
        #[serde(default)]
        pub generation: u64,
    }
}
