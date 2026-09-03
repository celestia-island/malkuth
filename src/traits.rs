//! Coordination, registry & election trait contracts (Layer 2 / Subsystems).
//!
//! These are the pluggable backends behind both fault-tolerance strategies:
//! - [`CoordinationLock`] is used by Subsystem A (Replica) to coordinate
//!   concurrent writes, and by Subsystem B (Leader/Follower) as the leader lease.
//! - [`InstanceRegistry`] tracks who is online / draining (rolling update).
//! - [`LeaderElector`] is the lease-election contract for active-passive HA.
//!
//! Concrete backends live in the `malkuth` crate (`lock` file-lock, `lease`
//! file-lease and `pg_lock` pg advisory-lock). Nothing in this module pulls
//! in a runtime or framework.

use std::time::Duration;

use async_trait::async_trait;
use thiserror::Error;

use crate::{InstanceInfo, InstanceRole, LeaderAnnounce};

// ═══════════════════════════════════════════════════════════════
// Coordination lock
// ═══════════════════════════════════════════════════════════════

/// Errors that can occur while acquiring or holding a lock.
#[derive(Debug, Error)]
pub enum LockError {
    /// The lock is held by another live owner.
    #[error("lock held by another owner: {0}")]
    Contended(String),
    /// An I/O error occurred.
    #[error("io error: {0}")]
    Io(#[from] std::io::Error),
    /// The backend is not built in (feature disabled).
    #[error("lock backend not available: {0}")]
    Unavailable(&'static str),
}

/// A held lock. Freed by an explicit [`release`](LockGuard::release); dropping
/// the guard is best-effort and backend-dependent (e.g. the pg advisory-lock
/// backend keeps the lock on its session until `release()` or the underlying
/// connection closes — dropping the guard alone does not unlock it).
#[async_trait]
pub trait LockGuard: Send + Sync {
    /// Release the lock explicitly.
    async fn release(&mut self);
}

/// A coordination lock backend.
#[async_trait]
pub trait CoordinationLock: Send + Sync {
    /// Acquire the lock named `key`, or fail immediately with
    /// [`LockError::Contended`] if another live owner holds it — no backend
    /// queues or waits.
    ///
    /// The `lease` parameter is **advisory** and its meaning differs per
    /// backend; callers must not assume the lock automatically expires:
    ///
    /// - `LeaseLock` (feature `lease`) honors it as the lease **TTL** — the
    ///   only backend with real expiry, renewed while the guard lives.
    /// - `FileLock` ignores it: the `flock` lives until `release()`, the
    ///   guard's `Drop` fallback, or process exit.
    /// - `PgLock` (feature `pg-lock`) ignores it: the session-level advisory
    ///   lock lives until `release()` or the connection closes.
    ///
    /// See each implementation's documentation for the exact semantics and
    /// for how its guard is freed.
    async fn acquire(&self, key: &str, lease: Duration) -> Result<Box<dyn LockGuard>, LockError>;
}

// ═══════════════════════════════════════════════════════════════
// Instance registry (Subsystem A)
// ═══════════════════════════════════════════════════════════════

/// Errors from the instance registry.
#[derive(Debug, Error)]
pub enum RegistryError {
    /// An instance with this id was not found.
    #[error("instance not found: {0}")]
    NotFound(String),
    /// The backing store could not be reached.
    #[error("registry store error: {0}")]
    Store(String),
}

/// A registry of the replicas in a group, used mainly during the
/// rolling-update window (a single record in steady state).
#[async_trait]
pub trait InstanceRegistry: Send + Sync {
    /// Insert or upsert this instance's entry.
    async fn register(&self, info: InstanceInfo) -> Result<(), RegistryError>;
    /// Update an instance's role (e.g. `Active` → `Draining`).
    async fn set_role(&self, instance_id: &str, role: InstanceRole) -> Result<(), RegistryError>;
    /// Remove an instance that has exited.
    async fn deregister(&self, instance_id: &str) -> Result<(), RegistryError>;
    /// List the instances currently known in `group`.
    async fn list(&self, group: &str) -> Result<Vec<InstanceInfo>, RegistryError>;
}

// ═══════════════════════════════════════════════════════════════
// Leader election (Subsystem B)
// ═══════════════════════════════════════════════════════════════

/// Errors during leader election.
#[derive(Debug, Error)]
pub enum ElectionError {
    /// The lease is currently held by another live candidate.
    #[error("lease contended: {0}")]
    Contended(String),
    /// The backing store could not be reached.
    #[error("election store error: {0}")]
    Store(String),
}

/// Lease-based leader election.
#[async_trait]
pub trait LeaderElector: Send + Sync {
    /// Try to acquire leadership for `ttl`. Returns `true` if this candidate
    /// is now the leader.
    async fn try_acquire(&self, ttl: Duration) -> Result<bool, ElectionError>;
    /// Renew the held lease. Returns `false` if leadership was lost.
    async fn renew(&self) -> Result<bool, ElectionError>;
    /// Best-effort query of the current leader, if any.
    async fn current(&self) -> Result<Option<LeaderAnnounce>, ElectionError>;
    /// Step down voluntarily (e.g. before a rolling update).
    async fn resign(&self) -> Result<(), ElectionError>;
}
