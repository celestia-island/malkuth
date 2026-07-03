//! Pluggable lifecycle hooks (requirement: "exit probes / heartbeats are
//! optional, hookable — use the default, or supply your own").
//!
//! Every default facility in the `malkuth` crate (signal-based exit, HTTP
//! probes, timed heartbeats) is wired through one of these traits. If the
//! default does not fit — e.g. you want drain to be triggered when your server
//! receives an application-level "stop" command, or you want heartbeats to
//! carry your own payload — implement the trait and hand it to the supervisor.
//! Nothing here depends on a runtime or a wire framework.

use std::time::Duration;

use async_trait::async_trait;

use crate::{DrainController, HealthStatus, HeartbeatBeat, ReadyStatus, ShutdownKind};

/// Why an [`ExitSource`] fired.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct ExitReason {
    /// The kind of shutdown implied.
    pub kind: ShutdownKind,
    /// Whether the process should actually exit once drain completes
    /// (`false` for a pure reload that keeps the process alive).
    pub should_exit: bool,
}

impl ExitReason {
    /// A graceful drain that should terminate the process.
    pub const fn graceful() -> Self {
        Self {
            kind: ShutdownKind::Graceful,
            should_exit: true,
        }
    }
    /// An immediate (skip-drain) exit.
    pub const fn immediate() -> Self {
        Self {
            kind: ShutdownKind::Immediate,
            should_exit: true,
        }
    }
    /// A reload: drain bit stays clear, process keeps running.
    pub const fn reload() -> Self {
        Self {
            kind: ShutdownKind::Reload,
            should_exit: false,
        }
    }
}

/// Source of process-exit / drain triggers.
///
/// The default implementation (in the `malkuth` crate) installs OS signal
/// handlers (`SIGTERM`/`SIGINT`/`SIGHUP`/`SIGQUIT`). A custom implementation
/// might instead trigger drain when the server receives an in-band "stop"
/// command, when a parent supervisor sends a control message over IPC, or when
/// an orchestrator flips a file bit.
///
/// Implementations **must** call `ctrl.begin_drain(reason.kind)` (and arrange
/// process exit if `reason.should_exit`) when their condition fires.
#[async_trait]
pub trait ExitSource: Send + Sync {
    /// Block until the source fires, then describe why.
    ///
    /// Returning is the signal that drain should begin; the caller then drives
    /// [`DrainController::wait_for_drain`] and exits if asked.
    async fn wait(&self, ctrl: DrainController) -> ExitReason;
}

/// Readiness/liveness state as observed by probes (HTTP `/readyz` or RPC
/// `Lifecycle.Status`). Implementations decide *how* the state is computed
/// (read atomics, ping a dependency, query a registry, …).
#[async_trait]
pub trait ProbeSink: Send + Sync {
    /// Current readiness (drain bit + dependency checks).
    async fn ready(&self) -> ReadyStatus;
    /// Current liveness.
    async fn health(&self) -> HealthStatus;
}

/// Heartbeat report produced by a [`Heartbeat`] source.
#[derive(Debug, Clone, Default)]
pub struct HeartbeatReport {
    /// The beat to publish, if any.
    pub beat: Option<HeartbeatBeat>,
    /// Interval until the next beat should be sampled.
    pub next_after: Duration,
}

/// A periodic liveness heartbeat. The default implementation emits a beat on a
/// fixed cadence; a custom one can embed domain-specific state, rate-limit, or
/// suppress beats conditionally.
#[async_trait]
pub trait Heartbeat: Send + Sync {
    /// Produce the next heartbeat report (blocking until it is due).
    async fn next(&self) -> HeartbeatReport;
}

/// A drain hook: run arbitrary graceful-shutdown work when drain begins.
///
/// Wired in by the supervisor between "stop accepting new work" and "exit".
/// Examples: close WebSocket frames, flush buffers, disconnect upstream pools,
/// release locks. This is the seam where each application injects its own
/// drain closure (the design doc §5.3 step 3–6).
#[async_trait]
pub trait DrainHook: Send + Sync {
    /// Perform drain work, completing within `budget` where possible.
    async fn drain(&self, budget: Duration);
}
