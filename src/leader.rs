//! Subsystem B — leader/follower (edge active-passive HA).
//!
//! On a device (notably an evernight gateway) two processes run as
//! leader/follower for fault tolerance. The leader exclusively owns the
//! physical I/O (PLC/serial/CAN connections); the follower stands by and
//! promotes only when the leader's lease expires. Only the supervisor is
//! made HA — per-resource workers are simply restarted on crash
//! (`WorkerSpec` + `Supervisor`), the OTP let-it-crash simplification.
//!
//! The [`LeaderElector`] trait below fixes the lease-election contract.
//! Concrete backends (a lease file + TTL, or a DB advisory lock) are staged
//! for a later phase.

use std::time::Duration;

use async_trait::async_trait;
use thiserror::Error;

use crate::LeaderAnnounce;

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
///
/// A candidate tries to acquire the lease for `ttl`; while leader it must
/// `renew` before the TTL elapses, otherwise a follower may promote itself.
/// Fencing is provided by the exclusivity of the backing lock.
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
