//! Worker supervision — a supervised child-process resource.
//!
//! Each [`WorkerSpec`] describes one independently-killable child process
//! that holds exactly one resource (a PLC connection, a serial port, a
//! sidecar like cosmos / pglite-proxy). The process is the failure-isolation
//! boundary. [`Supervisor`] spawns the workers and restarts them per their
//! [`RestartPolicy`] (`permanent` / `transient` / `temporary`), with a
//! sliding-window rate limit to prevent crash storms.

use std::time::{Duration, Instant};

use crate::{RestartPolicy, WorkerInfo, WorkerStatus};
use tokio::process::{Child, Command};
use tokio::sync::watch;
use tracing::{info, warn};

/// Default sliding window for restart rate-limiting.
pub const DEFAULT_WINDOW: Duration = Duration::from_secs(60);
/// Default max restarts within the window before entering cooldown.
pub const DEFAULT_MAX_RESTARTS: u32 = 5;
/// Default cooldown after the rate limit trips.
pub const DEFAULT_COOLDOWN: Duration = Duration::from_secs(30);

/// Specification of one supervised worker.
#[derive(Clone)]
pub struct WorkerSpec {
    /// Worker identifier (unique within its supervisor).
    pub id: String,
    /// Resource kind this worker holds (e.g. `modbus`, `s7comm`, `cosmos`).
    pub kind: String,
    /// Executable path / program name.
    pub program: String,
    /// Program arguments.
    pub args: Vec<String>,
    /// Restart policy. Defaults to [`RestartPolicy::Permanent`].
    pub restart_policy: RestartPolicy,
}

impl WorkerSpec {
    /// Builder entry point.
    #[must_use]
    pub fn new(id: impl Into<String>, kind: impl Into<String>, program: impl Into<String>) -> Self {
        Self {
            id: id.into(),
            kind: kind.into(),
            program: program.into(),
            args: Vec::new(),
            restart_policy: RestartPolicy::Permanent,
        }
    }

    #[must_use]
    pub fn args<I, S>(mut self, args: I) -> Self
    where
        I: IntoIterator<Item = S>,
        S: Into<String>,
    {
        self.args = args.into_iter().map(Into::into).collect();
        self
    }

    #[must_use]
    pub fn policy(mut self, policy: RestartPolicy) -> Self {
        self.restart_policy = policy;
        self
    }
}

/// Owns a pool of workers and supervises them.
pub struct Supervisor {
    specs: Vec<WorkerSpec>,
    max_restarts: u32,
    window: Duration,
    cooldown: Duration,
}

impl Supervisor {
    /// Create a supervisor for the given worker specs.
    #[must_use]
    pub fn new(specs: Vec<WorkerSpec>) -> Self {
        Self {
            specs,
            max_restarts: DEFAULT_MAX_RESTARTS,
            window: DEFAULT_WINDOW,
            cooldown: DEFAULT_COOLDOWN,
        }
    }

    /// Override the restart rate-limit (`max_restarts` within `window`).
    #[must_use]
    pub fn rate_limit(mut self, max_restarts: u32, window: Duration) -> Self {
        self.max_restarts = max_restarts;
        self.window = window;
        self
    }

    /// Override the cooldown applied when the rate limit trips.
    #[must_use]
    pub fn cooldown(mut self, cooldown: Duration) -> Self {
        self.cooldown = cooldown;
        self
    }

    /// Run the supervision loop until `shutdown` receives `true`, then kill
    /// all workers and return their final status snapshots.
    ///
    /// Each worker runs in its own task; a crash is restarted subject to the
    /// policy and the sliding-window rate limit.
    pub async fn run(self, shutdown: watch::Receiver<bool>) -> Vec<WorkerInfo> {
        let mut handles = Vec::new();
        for spec in self.specs {
            let mut shutdown_rx = shutdown.clone();
            let max_restarts = self.max_restarts;
            let window = self.window;
            let cooldown = self.cooldown;
            handles.push(tokio::spawn(async move {
                supervise_one(spec, max_restarts, window, cooldown, &mut shutdown_rx).await
            }));
        }

        // Wait for shutdown signal, then join all supervision tasks (each
        // observes shutdown_rx and returns).
        let mut shutdown = shutdown;
        let _ = shutdown.wait_for(|&d| d).await;
        let mut results = Vec::with_capacity(handles.len());
        for h in handles {
            match h.await {
                Ok(info) => results.push(info),
                Err(e) => warn!(error = %e, "supervision task panicked"),
            }
        }
        results
    }
}

async fn supervise_one(
    spec: WorkerSpec,
    max_restarts: u32,
    window: Duration,
    cooldown: Duration,
    shutdown_rx: &mut watch::Receiver<bool>,
) -> WorkerInfo {
    let mut restart_count: u32 = 0;
    let mut restart_times: Vec<Instant> = Vec::new();
    let mut last_error: Option<String> = None;

    loop {
        if *shutdown_rx.borrow() {
            break;
        }

        let mut child = match spawn(&spec) {
            Ok(c) => c,
            Err(e) => {
                last_error = Some(format!("spawn failed: {e}"));
                warn!(worker = %spec.id, error = %e, "failed to spawn worker");
                if !should_restart(spec.restart_policy, false) {
                    break;
                }
                if !rate_limited(&mut restart_times, max_restarts, window, cooldown).await {
                    restart_count += 1;
                }
                continue;
            }
        };

        // Race child exit against shutdown.
        let exit = tokio::select! {
            r = child.wait() => r,
            _ = shutdown_rx.wait_for(|&d| d) => break,
        };

        match exit {
            Ok(status) => {
                let clean = status.success();
                info!(worker = %spec.id, clean, "worker exited");
                if !should_restart(spec.restart_policy, clean) {
                    break;
                }
            }
            Err(e) => {
                last_error = Some(format!("wait failed: {e}"));
                warn!(worker = %spec.id, error = %e, "failed to await worker");
            }
        }

        restart_count += 1;
        if rate_limited(&mut restart_times, max_restarts, window, cooldown).await {
            break;
        }
    }

    WorkerInfo {
        id: spec.id.clone(),
        kind: spec.kind.clone(),
        status: WorkerStatus::Stopped,
        restart_policy: spec.restart_policy,
        restart_count,
        last_error,
    }
}

fn spawn(spec: &WorkerSpec) -> std::io::Result<Child> {
    let mut cmd = Command::new(&spec.program);
    cmd.args(&spec.args);
    cmd.kill_on_drop(true);
    cmd.spawn()
}

/// OTP semantics: permanent always restarts; transient only on abnormal
/// exit; temporary never.
fn should_restart(policy: RestartPolicy, clean_exit: bool) -> bool {
    match policy {
        RestartPolicy::Permanent => true,
        RestartPolicy::Transient => !clean_exit,
        RestartPolicy::Temporary => false,
    }
}

/// Push now into the sliding window; if the count exceeds `max_restarts`
/// within `window`, sleep `cooldown` and return true (rate-limited).
async fn rate_limited(
    restart_times: &mut Vec<Instant>,
    max_restarts: u32,
    window: Duration,
    cooldown: Duration,
) -> bool {
    let now = Instant::now();
    restart_times.retain(|t| now.duration_since(*t) < window);
    restart_times.push(now);
    if restart_times.len() as u32 > max_restarts {
        warn!(
            restarts = restart_times.len(),
            "restart rate limit tripped, entering cooldown"
        );
        tokio::time::sleep(cooldown).await;
        restart_times.clear();
        true
    } else {
        false
    }
}

/// `restart_times` is retained per worker so callers/tests can inspect the
/// rate-limit window; the supervision loop itself does not yet publish
/// per-transition status snapshots (kept minimal for the first cut).
#[allow(dead_code)]
fn _touch_restart_times(restart_times: &[Instant]) -> usize {
    restart_times.len()
}
