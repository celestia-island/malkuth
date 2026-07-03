//! Supervised child-process workers (tokio).
//!
//! Each [`WorkerSpec`] is one independently-killable child process holding one
//! resource. [`Supervisor`] spawns them and restarts per their
//! [`RestartPolicy`] (`permanent` / `transient` / `temporary`), with a
//! sliding-window rate limit to prevent crash storms. Workers are supervised
//! concurrently via a `FuturesUnordered`.

use std::process::Stdio;
use std::time::{Duration, Instant};

use crate::{DrainController, RestartPolicy, WorkerInfo, WorkerStatus};
use futures_util::stream::{FuturesUnordered, StreamExt};
use tokio::process::{Child, Command};
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
    pub id: String,
    pub kind: String,
    pub program: String,
    pub args: Vec<String>,
    pub env: Vec<(String, String)>,
    pub restart_policy: RestartPolicy,
}

impl WorkerSpec {
    #[must_use]
    pub fn new(id: impl Into<String>, kind: impl Into<String>, program: impl Into<String>) -> Self {
        Self {
            id: id.into(),
            kind: kind.into(),
            program: program.into(),
            args: Vec::new(),
            env: Vec::new(),
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
    pub fn env<K, V>(mut self, key: K, value: V) -> Self
    where
        K: Into<String>,
        V: Into<String>,
    {
        self.env.push((key.into(), value.into()));
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
    #[must_use]
    pub fn new(specs: Vec<WorkerSpec>) -> Self {
        Self {
            specs,
            max_restarts: DEFAULT_MAX_RESTARTS,
            window: DEFAULT_WINDOW,
            cooldown: DEFAULT_COOLDOWN,
        }
    }
    #[must_use]
    pub fn rate_limit(mut self, max_restarts: u32, window: Duration) -> Self {
        self.max_restarts = max_restarts;
        self.window = window;
        self
    }
    #[must_use]
    pub fn cooldown(mut self, cooldown: Duration) -> Self {
        self.cooldown = cooldown;
        self
    }

    /// Run the supervision loop until `drain` begins, then return final snapshots.
    pub async fn run(self, drain: DrainController) -> Vec<WorkerInfo> {
        let mut handles: FuturesUnordered<
            std::pin::Pin<Box<dyn std::future::Future<Output = WorkerInfo> + Send>>,
        > = FuturesUnordered::new();
        for spec in self.specs {
            let drain = drain.clone();
            let (max_restarts, window, cooldown) = (self.max_restarts, self.window, self.cooldown);
            handles.push(Box::pin(async move {
                supervise_one(spec, max_restarts, window, cooldown, drain).await
            })
                as std::pin::Pin<
                    Box<dyn std::future::Future<Output = WorkerInfo> + Send>,
                >);
        }
        let mut results = Vec::new();
        while let Some(info) = handles.next().await {
            results.push(info);
        }
        results
    }
}

async fn supervise_one(
    spec: WorkerSpec,
    max_restarts: u32,
    window: Duration,
    cooldown: Duration,
    drain: DrainController,
) -> WorkerInfo {
    let mut restart_count: u32 = 0;
    let mut restart_times: Vec<Instant> = Vec::new();
    let mut last_error: Option<String> = None;

    loop {
        if drain.is_draining() {
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
                if rate_limited(&mut restart_times, max_restarts, window, cooldown, &drain).await {
                    break;
                }
                restart_count += 1;
                continue;
            }
        };

        // Race child exit against drain.
        tokio::select! {
            status = child.wait() => {
                match status {
                    Ok(s) => {
                        let clean = s.success();
                        info!(worker = %spec.id, clean, "worker exited");
                        if !should_restart(spec.restart_policy, clean) { break; }
                    }
                    Err(e) => {
                        last_error = Some(format!("wait failed: {e}"));
                        warn!(worker = %spec.id, error = %e, "failed to await worker");
                    }
                }
                restart_count += 1;
                if rate_limited(&mut restart_times, max_restarts, window, cooldown, &drain).await {
                    break;
                }
            }
            _ = drain.wait_for_drain() => {
                // Best-effort kill on drain.
                let _ = child.kill().await;
                break;
            }
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
    for (k, v) in &spec.env {
        cmd.env(k, v);
    }
    cmd.stdin(Stdio::null());
    cmd.stdout(Stdio::inherit());
    cmd.stderr(Stdio::inherit());
    cmd.kill_on_drop(true);
    cmd.spawn()
}

fn should_restart(policy: RestartPolicy, clean_exit: bool) -> bool {
    match policy {
        RestartPolicy::Permanent => true,
        RestartPolicy::Transient => !clean_exit,
        RestartPolicy::Temporary => false,
    }
}

/// Returns true if the rate limit tripped (after sleeping the cooldown).
async fn rate_limited(
    restart_times: &mut Vec<Instant>,
    max_restarts: u32,
    window: Duration,
    cooldown: Duration,
    drain: &DrainController,
) -> bool {
    let now = Instant::now();
    restart_times.retain(|t| now.duration_since(*t) < window);
    restart_times.push(now);
    if restart_times.len() as u32 > max_restarts {
        warn!(
            restarts = restart_times.len(),
            "restart rate limit tripped, entering cooldown"
        );
        tokio::select! {
            _ = tokio::time::sleep(cooldown) => {}
            _ = drain.wait_for_drain() => {}
        }
        restart_times.clear();
        true
    } else {
        false
    }
}
