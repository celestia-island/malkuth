//! Subsystem A — peer replicas (load-balancing ⊃ rolling update).
//!
//! Peers are equal and active-active; state is externalised to a shared
//! store (Postgres for entelecheia / shittim-chest), so replicas need no
//! state replication between them — only a registry of who is online and
//! who is draining. Rolling update is a maintenance operation of this
//! subsystem: add a new replica (new generation) → wait `/readyz` → mark an
//! old replica `Draining` → let it finish and exit → repeat.
//!
//! The registry trait below is the contract backends (a shared DB table, or
//! a file-based store for single-host deployments) will implement. Full
//! backends + the manifest-queue orchestrator are staged for a later phase.

use async_trait::async_trait;
use thiserror::Error;

use crate::{InstanceInfo, InstanceRole};

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
///
/// Implementations may back this with a Postgres table (entelecheia /
/// shittim-chest) or a JSON file on a shared volume.
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
