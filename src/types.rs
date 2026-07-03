//! Lifecycle, supervision & rolling-update wire types.
//!
//! These are the protocol types that cross process boundaries (JSON-RPC over
//! loopback / WebSocket / IPC, HTTP probes, instance-registry queries). The
//! matching runtime behaviour lives in the `malkuth` crate.

use serde::{Deserialize, Serialize};

// ═══════════════════════════════════════════════════════════════
// JSON-RPC method names
// ═══════════════════════════════════════════════════════════════

/// JSON-RPC method names used by the lifecycle / supervision protocol.
pub mod methods {
    pub const DRAIN: &str = "Lifecycle.Drain";
    pub const RELOAD: &str = "Lifecycle.Reload";
    pub const STATUS: &str = "Lifecycle.Status";
    pub const HEALTH: &str = "Lifecycle.Health";
    pub const WORKER_STATUS: &str = "Worker.Status";
    pub const INSTANCE_REGISTER: &str = "Lifecycle.InstanceRegister";
    pub const INSTANCE_DEREGISTER: &str = "Lifecycle.InstanceDeregister";
    pub const INSTANCE_LIST: &str = "Lifecycle.InstanceList";
    pub const LEADER_ANNOUNCE: &str = "Lifecycle.LeaderAnnounce";
    pub const HEARTBEAT: &str = "Lifecycle.Heartbeat";
}

// ═══════════════════════════════════════════════════════════════
// Drain state
// ═══════════════════════════════════════════════════════════════

/// High-level lifecycle state of an instance, observable over the wire.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default, Serialize, Deserialize)]
#[cfg_attr(feature = "schema", derive(schemars::JsonSchema))]
#[serde(rename_all = "snake_case")]
pub enum DrainState {
    #[default]
    Active,
    Draining,
    Reloading,
}

// ═══════════════════════════════════════════════════════════════
// Health probes
// ═══════════════════════════════════════════════════════════════

/// Result of one dependency check reported by `/readyz`.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[cfg_attr(feature = "schema", derive(schemars::JsonSchema))]
pub struct DependencyCheck {
    pub name: String,
    pub ok: bool,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub detail: Option<String>,
}

/// Readiness probe payload — the body of `GET /readyz`. The `draining` flag is
/// the central rolling-update signal: route new traffic only to instances whose
/// `ready && !draining`.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[cfg_attr(feature = "schema", derive(schemars::JsonSchema))]
pub struct ReadyStatus {
    pub ready: bool,
    pub draining: bool,
    #[serde(default)]
    pub dependencies: Vec<DependencyCheck>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub generation: Option<u64>,
}

/// Liveness probe payload — the body of `GET /healthz`.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[cfg_attr(feature = "schema", derive(schemars::JsonSchema))]
pub struct HealthStatus {
    pub alive: bool,
    pub pid: u32,
    pub uptime_secs: u64,
    pub version: String,
}

// ═══════════════════════════════════════════════════════════════
// Instance registry
// ═══════════════════════════════════════════════════════════════

/// Role of an instance within its group.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[cfg_attr(feature = "schema", derive(schemars::JsonSchema))]
#[serde(rename_all = "snake_case")]
pub enum InstanceRole {
    Active,
    Draining,
}

/// One entry in the shared instance registry.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[cfg_attr(feature = "schema", derive(schemars::JsonSchema))]
pub struct InstanceInfo {
    pub instance_id: String,
    pub group: String,
    pub role: InstanceRole,
    pub generation: u64,
    pub started_at: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub endpoint: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub backend: Option<String>,
}

// ═══════════════════════════════════════════════════════════════
// Leader / follower (Subsystem B)
// ═══════════════════════════════════════════════════════════════

/// Leader/follower role for Subsystem B (active-passive HA).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[cfg_attr(feature = "schema", derive(schemars::JsonSchema))]
#[serde(rename_all = "snake_case")]
pub enum LeaderRole {
    Leader,
    Follower,
}

/// Announcement of a lease acquisition or transfer.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[cfg_attr(feature = "schema", derive(schemars::JsonSchema))]
pub struct LeaderAnnounce {
    pub node_id: String,
    pub leader_instance_id: String,
    pub term: u64,
    pub acquired_at: String,
    pub lease_ttl_secs: u32,
}

// ═══════════════════════════════════════════════════════════════
// Worker supervision
// ═══════════════════════════════════════════════════════════════

/// Lifecycle state of one supervised worker process.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[cfg_attr(feature = "schema", derive(schemars::JsonSchema))]
#[serde(rename_all = "snake_case")]
pub enum WorkerStatus {
    Starting,
    Running,
    Stopped,
    Failed,
}

/// When to restart a worker after it exits (OTP vocabulary).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default, Serialize, Deserialize)]
#[cfg_attr(feature = "schema", derive(schemars::JsonSchema))]
#[serde(rename_all = "snake_case")]
pub enum RestartPolicy {
    #[default]
    Permanent,
    Transient,
    Temporary,
}

/// Snapshot of one worker, reported over `Worker.Status`.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[cfg_attr(feature = "schema", derive(schemars::JsonSchema))]
pub struct WorkerInfo {
    pub id: String,
    pub kind: String,
    pub status: WorkerStatus,
    pub restart_policy: RestartPolicy,
    #[serde(default)]
    pub restart_count: u32,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub last_error: Option<String>,
}

// ═══════════════════════════════════════════════════════════════
// RPC request/response bodies
// ═══════════════════════════════════════════════════════════════

/// Body of `Lifecycle.Drain`.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[cfg_attr(feature = "schema", derive(schemars::JsonSchema))]
pub struct DrainRequest {
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub timeout_secs: Option<u32>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub reason: Option<String>,
}

/// Reply to `Lifecycle.Drain`.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[cfg_attr(feature = "schema", derive(schemars::JsonSchema))]
pub struct DrainResponse {
    pub accepted: bool,
    pub draining: bool,
}

/// Body of `Lifecycle.Heartbeat`.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[cfg_attr(feature = "schema", derive(schemars::JsonSchema))]
pub struct HeartbeatBeat {
    pub instance_id: String,
    pub group: String,
    pub ts: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub backend: Option<String>,
    #[serde(default)]
    pub generation: u64,
}
