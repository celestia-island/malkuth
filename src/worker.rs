//! Supervised child-process workers (tokio).
//!
//! Each [`WorkerSpec`] is one independently-killable child process holding one
//! resource. [`Supervisor`] spawns them and restarts per their
//! [`RestartPolicy`] (`permanent` / `transient` / `temporary`), with a
//! sliding-window rate limit to prevent crash storms. Workers are supervised
//! concurrently via a `FuturesUnordered`.

use std::{
    collections::{HashMap, VecDeque},
    path::PathBuf,
    process::Stdio,
    sync::{Arc, Mutex},
    time::{Duration, Instant, SystemTime},
};
use tokio::io::{AsyncBufReadExt, BufReader};
use tokio::process::{Child, Command};

use futures_util::stream::{FuturesUnordered, StreamExt};
use tracing::{info, warn};

use crate::{
    DrainController, RestartPolicy, WorkerInfo, WorkerPhase, WorkerSnapshot, WorkerStatus,
};

/// Default sliding window for restart rate-limiting.
pub const DEFAULT_WINDOW: Duration = Duration::from_secs(60);
/// Default max restarts within the window before entering cooldown.
pub const DEFAULT_MAX_RESTARTS: u32 = 5;
/// Default cooldown after the rate limit trips.
pub const DEFAULT_COOLDOWN: Duration = Duration::from_secs(30);

/// Lines kept per worker by [`WorkerLogRing`] (the same 500-line cap the CLI
/// pool's in-memory VTTY ring uses).
pub const LOG_RING_CAPACITY: usize = 500;

/// Longest single captured log line, in bytes. A runaway child can emit
/// megabyte-long lines; truncating at the ring boundary keeps the supervisor
/// from growing without bound. Truncated lines carry a trailing `…`.
pub const LOG_LINE_MAX_BYTES: usize = 8 * 1024;

/// Bounded ring of one worker's recent stdout/stderr lines.
///
/// Capturing is opt-in per worker ([`WorkerSpec::capture_logs`]) because the
/// default is `Stdio::inherit()` — journald (or the parent's terminal) owns
/// child output in the normal deployment, and piping it here would silently
/// take it away. A ring is what the `logs` arm serves: the *tail only*, with
/// its bounds visible to the caller.
#[derive(Clone, Default)]
pub struct WorkerLogRing {
    inner: Arc<Mutex<VecDeque<String>>>,
}

impl WorkerLogRing {
    #[must_use]
    pub fn new() -> Self {
        Self::default()
    }

    /// Append one line, truncating it at [`LOG_LINE_MAX_BYTES`].
    pub fn push(&self, line: &str) {
        let trimmed = line.trim_end();
        if trimmed.is_empty() {
            return;
        }
        let mut owned = trimmed.to_string();
        if owned.len() > LOG_LINE_MAX_BYTES {
            let mut cut = LOG_LINE_MAX_BYTES;
            while cut > 0 && !owned.is_char_boundary(cut) {
                cut -= 1;
            }
            owned.truncate(cut);
            owned.push('…');
        }
        let Ok(mut ring) = self.inner.lock() else {
            return;
        };
        if ring.len() == LOG_RING_CAPACITY {
            ring.pop_front();
        }
        ring.push_back(owned);
    }

    /// The newest `max_lines` lines, oldest first.
    #[must_use]
    pub fn tail(&self, max_lines: usize) -> Vec<String> {
        let Ok(ring) = self.inner.lock() else {
            return Vec::new();
        };
        let skip = ring.len().saturating_sub(max_lines);
        ring.iter().skip(skip).cloned().collect()
    }

    #[must_use]
    pub fn len(&self) -> usize {
        self.inner.lock().map(|r| r.len()).unwrap_or(0)
    }

    #[must_use]
    pub fn is_empty(&self) -> bool {
        self.len() == 0
    }
}

/// Live fleet snapshot sink published by a running [`Supervisor`].
///
/// Cheap to clone (one `Arc`); every supervised worker publishes its phase
/// transitions here, and any observer can read a consistent snapshot at any
/// time without touching the supervision loop. The supervisor writes through
/// the same struct it hands to observers, so there is no second copy of the
/// truth to drift.
#[derive(Clone, Default)]
pub struct FleetState {
    entries: Arc<Mutex<HashMap<String, FleetEntry>>>,
}

#[derive(Clone)]
struct FleetEntry {
    snapshot: WorkerSnapshot,
    /// When the *current* child was spawned; `None` until it starts.
    spawned_at: Option<SystemTime>,
}

impl FleetState {
    #[must_use]
    pub fn new() -> Self {
        Self::default()
    }

    /// Register a worker in `starting` phase (before its first spawn).
    pub fn register(&self, spec: &WorkerSpec) {
        let entry = FleetEntry {
            snapshot: WorkerSnapshot {
                id: spec.id.clone(),
                kind: spec.kind.clone(),
                phase: WorkerPhase::Starting,
                pid: None,
                uptime_secs: 0,
                restart_policy: spec.restart_policy,
                restart_count: 0,
                last_error: None,
            },
            spawned_at: None,
        };
        if let Ok(mut map) = self.entries.lock() {
            map.insert(spec.id.clone(), entry);
        }
    }

    /// Mark a worker running under `pid` (a fresh child just spawned).
    pub fn running(&self, id: &str, pid: Option<u32>) {
        self.update(id, |entry| {
            entry.snapshot.phase = WorkerPhase::Running;
            entry.snapshot.pid = pid;
            entry.snapshot.uptime_secs = 0;
            entry.spawned_at = Some(SystemTime::now());
        });
    }

    /// Mark a worker as restarting (its child was stopped for a restart).
    pub fn restarting(&self, id: &str, restart_count: u32) {
        self.update(id, |entry| {
            entry.snapshot.phase = WorkerPhase::Restarting;
            entry.snapshot.restart_count = restart_count;
            entry.snapshot.pid = None;
            entry.snapshot.uptime_secs = 0;
            entry.spawned_at = None;
        });
    }

    /// Mark a worker as draining (stop requested, child still finishing).
    pub fn draining(&self, id: &str) {
        self.update(id, |entry| {
            entry.snapshot.phase = WorkerPhase::Draining;
            entry.snapshot.pid = None;
        });
    }

    /// Publish a terminal phase plus the run's final counters.
    pub fn finish(
        &self,
        id: &str,
        phase: WorkerPhase,
        restart_count: u32,
        last_error: Option<String>,
    ) {
        self.update(id, |entry| {
            entry.snapshot.phase = phase;
            entry.snapshot.restart_count = restart_count;
            entry.snapshot.last_error = last_error;
            entry.snapshot.pid = None;
            entry.snapshot.uptime_secs = 0;
            entry.spawned_at = None;
        });
    }

    fn update(&self, id: &str, f: impl FnOnce(&mut FleetEntry)) {
        let Ok(mut map) = self.entries.lock() else {
            return;
        };
        if let Some(entry) = map.get_mut(id) {
            f(entry);
        }
    }

    fn with_uptime(entry: &FleetEntry) -> WorkerSnapshot {
        let mut snapshot = entry.snapshot.clone();
        if let Some(spawned_at) = entry.spawned_at {
            snapshot.uptime_secs = SystemTime::now()
                .duration_since(spawned_at)
                .map(|d| d.as_secs())
                .unwrap_or(0);
        }
        snapshot
    }

    /// One worker's current snapshot, if it is registered.
    #[must_use]
    pub fn get(&self, id: &str) -> Option<WorkerSnapshot> {
        let map = self.entries.lock().ok()?;
        map.get(id).map(Self::with_uptime)
    }

    /// Every registered worker, sorted by id (deterministic tool output).
    #[must_use]
    pub fn snapshot(&self) -> Vec<WorkerSnapshot> {
        let Ok(map) = self.entries.lock() else {
            return Vec::new();
        };
        let mut out: Vec<WorkerSnapshot> = map.values().map(Self::with_uptime).collect();
        out.sort_by(|a, b| a.id.cmp(&b.id));
        out
    }
}

/// Specification of one supervised worker.
#[derive(Clone)]
pub struct WorkerSpec {
    pub id: String,
    pub kind: String,
    pub program: String,
    pub args: Vec<String>,
    pub env: Vec<(String, String)>,
    pub restart_policy: RestartPolicy,
    /// Optional working directory for the child process.
    pub working_dir: Option<PathBuf>,
    /// Per-worker drain signal — set externally to trigger a graceful restart
    /// of this specific worker without affecting the rest of the pool.
    #[allow(clippy::type_complexity)]
    pub drain_signal: Option<Arc<tokio::sync::Notify>>,
    /// Per-worker restart signal — stops the current child and spawns a fresh
    /// one (the child is re-created, unlike `drain_signal` which ends the
    /// worker for good).
    #[allow(clippy::type_complexity)]
    pub restart_signal: Option<Arc<tokio::sync::Notify>>,
    /// Optional bounded capture of this worker's stdout/stderr. `None` keeps
    /// the historical `Stdio::inherit()` behaviour.
    pub log_ring: Option<WorkerLogRing>,
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
            working_dir: None,
            drain_signal: None,
            restart_signal: None,
            log_ring: None,
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
    #[must_use]
    pub fn working_dir(mut self, dir: impl Into<PathBuf>) -> Self {
        self.working_dir = Some(dir.into());
        self
    }

    /// Attach a per-worker restart signal. `notify_one()` on the returned
    /// handle stops the current child and spawns a fresh one; the worker
    /// stays in the pool (contrast [`Self::drain_signal`], which ends it).
    #[must_use]
    pub fn restart_signal(mut self, signal: Arc<tokio::sync::Notify>) -> Self {
        self.restart_signal = Some(signal);
        self
    }

    /// Capture this worker's stdout/stderr into a bounded ring (see
    /// [`WorkerLogRing`]) instead of inheriting the parent's handles.
    #[must_use]
    pub fn capture_logs(mut self, ring: WorkerLogRing) -> Self {
        self.log_ring = Some(ring);
        self
    }
}

/// Owns a pool of workers and supervises them.
pub struct Supervisor {
    specs: Vec<WorkerSpec>,
    max_restarts: u32,
    window: Duration,
    cooldown: Duration,
    fleet: Option<FleetState>,
}

impl Supervisor {
    #[must_use]
    pub fn new(specs: Vec<WorkerSpec>) -> Self {
        Self {
            specs,
            max_restarts: DEFAULT_MAX_RESTARTS,
            window: DEFAULT_WINDOW,
            cooldown: DEFAULT_COOLDOWN,
            fleet: None,
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

    /// Publish live worker state into `fleet` while the run is in flight.
    ///
    /// Without a sink the supervisor behaves exactly as before (no shared
    /// state, no extra allocations); with one, every registered worker
    /// publishes `starting → running → restarting/draining → stopped` so a
    /// concurrent reader can answer "what is alive right now, since when,
    /// and how many times has it restarted".
    #[must_use]
    pub fn fleet(mut self, fleet: FleetState) -> Self {
        self.fleet = Some(fleet);
        self
    }

    /// Register a drain signal for a worker and return a handle to trigger it.
    ///
    /// Call `notify_one()` on the returned `Arc<Notify>` to gracefully restart
    /// that specific worker. Must be called before `run()`.
    pub fn register_drain(&mut self, worker_id: &str) -> Option<Arc<tokio::sync::Notify>> {
        let signal = Arc::new(tokio::sync::Notify::new());
        for spec in &mut self.specs {
            if spec.id == worker_id {
                spec.drain_signal = Some(signal.clone());
                return Some(signal);
            }
        }
        None
    }

    /// Run the supervision loop until `drain` begins, then return final snapshots.
    pub async fn run(self, drain: DrainController) -> Vec<WorkerInfo> {
        let mut handles: FuturesUnordered<
            std::pin::Pin<Box<dyn std::future::Future<Output = WorkerInfo> + Send>>,
        > = FuturesUnordered::new();
        for spec in self.specs {
            let drain = drain.clone();
            let (max_restarts, window, cooldown) = (self.max_restarts, self.window, self.cooldown);
            let fleet = self.fleet.clone();
            handles.push(Box::pin(async move {
                supervise_one(spec, max_restarts, window, cooldown, drain, fleet).await
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
    fleet: Option<FleetState>,
) -> WorkerInfo {
    let mut restart_count: u32 = 0;
    let mut restart_times: Vec<Instant> = Vec::new();
    let mut last_error: Option<String> = None;
    if let Some(fleet) = &fleet {
        fleet.register(&spec);
    }

    loop {
        if drain.is_draining() {
            break;
        }
        let mut child = match spawn(&spec) {
            Ok(c) => c,
            Err(e) => {
                last_error = Some(format!("spawn failed: {e}"));
                warn!(worker = %spec.id, error = %e, "failed to spawn worker");
                if let Some(fleet) = &fleet {
                    fleet.finish(
                        &spec.id,
                        WorkerPhase::Stopped,
                        restart_count,
                        last_error.clone(),
                    );
                }
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
        if let Some(fleet) = &fleet {
            fleet.running(&spec.id, child.id());
        }

        // Race child exit against global drain, per-worker drain (ends the
        // worker) and per-worker restart (respawns it).
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
                if let Some(fleet) = &fleet {
                    fleet.draining(&spec.id);
                }
                stop_draining(&mut child, 30).await;
                break;
            }
            _ = async {
                if let Some(ref signal) = spec.drain_signal {
                    signal.notified().await;
                } else {
                    std::future::pending::<()>().await;
                }
            } => {
                info!(worker = %spec.id, "per-worker drain signal received");
                if let Some(fleet) = &fleet {
                    fleet.draining(&spec.id);
                }
                stop_draining(&mut child, 30).await;
                break;
            }
            _ = async {
                if let Some(ref signal) = spec.restart_signal {
                    signal.notified().await;
                } else {
                    std::future::pending::<()>().await;
                }
            } => {
                info!(worker = %spec.id, "per-worker restart signal received");
                restart_count += 1;
                if let Some(fleet) = &fleet {
                    fleet.restarting(&spec.id, restart_count);
                }
                stop_draining(&mut child, 30).await;
                if rate_limited(&mut restart_times, max_restarts, window, cooldown, &drain).await {
                    last_error = Some("restart rate limit tripped".to_string());
                    break;
                }
            }
        }
    }

    if let Some(fleet) = &fleet {
        fleet.finish(
            &spec.id,
            WorkerPhase::Stopped,
            restart_count,
            last_error.clone(),
        );
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

/// Audit malkuth-R1-1: stop a managed worker with a drain escalation
/// chain — SIGTERM first so the service can finish in-flight work, wait
/// up to `drain_secs` (default 30s when no budget is wired; see
/// drain_auth.rs for the authorization gate follow-up), then SIGKILL
/// and reap. The old call sites used `kill()` (SIGKILL) directly,
/// dropping in-flight requests on every drain.
pub(crate) async fn stop_draining(child: &mut Child, drain_secs: u64) {
    if let Some(pid) = child.id() {
        send_graceful(pid);
    }
    match tokio::time::timeout(
        std::time::Duration::from_secs(drain_secs.max(1)),
        child.wait(),
    )
    .await
    {
        // Exited within the drain window (gracefully or not).
        Ok(_) => {}
        Err(_) => {
            // Grace window elapsed: escalate to SIGKILL and reap.
            let _ = child.start_kill();
            let _ = child.wait().await;
        }
    }
}

#[cfg(unix)]
fn send_graceful(pid: u32) {
    // SAFETY: plain signal send to a live child pid; no memory ops.
    unsafe {
        libc::kill(pid as libc::pid_t, libc::SIGTERM);
    }
}

#[cfg(windows)]
fn send_graceful(pid: u32) {
    use windows_sys::Win32::System::Console::{CTRL_BREAK_EVENT, GenerateConsoleCtrlEvent};
    // Best-effort: a console child spawned in its own process group sees
    // CTRL_BREAK as a graceful stop. Services have no console, so a failed
    // call is expected — the drain timeout still escalates to TerminateProcess.
    unsafe {
        GenerateConsoleCtrlEvent(CTRL_BREAK_EVENT, pid);
    }
}

fn spawn(spec: &WorkerSpec) -> std::io::Result<Child> {
    let mut cmd = Command::new(&spec.program);
    cmd.args(&spec.args);
    for (k, v) in &spec.env {
        cmd.env(k, v);
    }
    if let Some(ref dir) = spec.working_dir {
        cmd.current_dir(dir);
    }
    cmd.stdin(Stdio::null());
    // Capture is opt-in: inheriting the parent's handles keeps journald (or
    // the operator's terminal) as the log destination, which is the right
    // default for a deployed service. A ring takes the pipes over, so the
    // caller asked for it explicitly.
    if spec.log_ring.is_some() {
        cmd.stdout(Stdio::piped());
        cmd.stderr(Stdio::piped());
    } else {
        cmd.stdout(Stdio::inherit());
        cmd.stderr(Stdio::inherit());
    }
    cmd.kill_on_drop(true);
    #[cfg(windows)]
    {
        use std::os::windows::process::CommandExt;
        // CREATE_NEW_PROCESS_GROUP: makes CTRL_BREAK target this child only.
        cmd.as_std_mut().creation_flags(0x0000_0200);
    }
    let mut child = cmd.spawn()?;
    if let Some(ring) = spec.log_ring.clone() {
        if let Some(out) = child.stdout.take() {
            capture_stream(out, ring.clone(), &spec.id);
        }
        if let Some(err) = child.stderr.take() {
            capture_stream(err, ring, &spec.id);
        }
    }
    Ok(child)
}

/// Drain one child stream into the worker's ring until the stream closes.
///
/// The reader never stops early: a chatty child must not block on pipe
/// backpressure, so lines keep being consumed (and the ring keeps evicting)
/// for as long as the child is alive.
fn capture_stream(
    stream: impl tokio::io::AsyncRead + Unpin + Send + 'static,
    ring: WorkerLogRing,
    worker: &str,
) {
    let worker = worker.to_string();
    tokio::spawn(async move {
        let mut lines = BufReader::new(stream).lines();
        // Clean EOF (`Ok(None)`, the child exited) and a dead pipe (`Err`)
        // both end the capture — the loop must never spin on a broken stream.
        while let Ok(Some(line)) = lines.next_line().await {
            // Taking the pipe over must not cost the operator the log stream:
            // mirror each line to tracing (silenceable with
            // RUST_LOG=info,malkuth::worker=off) while the ring keeps the
            // bounded tail the `logs` arm serves.
            info!(target: "malkuth::worker", worker = %worker, "{line}");
            ring.push(&line);
        }
    });
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

#[cfg(test)]
mod tests {
    use super::*;
    use crate::ShutdownKind;

    fn spec(id: &str) -> WorkerSpec {
        WorkerSpec::new(id, "test", "/bin/true")
    }

    #[test]
    fn fleet_publishes_phase_transitions() {
        let fleet = FleetState::new();
        fleet.register(&spec("alpha"));

        let snap = fleet.get("alpha").expect("registered");
        assert_eq!(snap.phase, WorkerPhase::Starting);
        assert_eq!(snap.pid, None);
        assert_eq!(snap.uptime_secs, 0);

        fleet.running("alpha", Some(1234));
        let snap = fleet.get("alpha").expect("registered");
        assert_eq!(snap.phase, WorkerPhase::Running);
        assert_eq!(snap.pid, Some(1234));

        fleet.restarting("alpha", 1);
        let snap = fleet.get("alpha").expect("registered");
        assert_eq!(snap.phase, WorkerPhase::Restarting);
        assert_eq!(snap.restart_count, 1);
        assert_eq!(snap.pid, None);

        fleet.draining("alpha");
        assert_eq!(
            fleet.get("alpha").map(|s| s.phase),
            Some(WorkerPhase::Draining)
        );

        fleet.finish("alpha", WorkerPhase::Stopped, 2, Some("boom".into()));
        let snap = fleet.get("alpha").expect("registered");
        assert_eq!(snap.phase, WorkerPhase::Stopped);
        assert_eq!(snap.restart_count, 2);
        assert_eq!(snap.last_error.as_deref(), Some("boom"));
        assert_eq!(snap.pid, None);
    }

    #[test]
    fn fleet_snapshot_is_sorted_and_unknown_ids_are_invisible() {
        let fleet = FleetState::new();
        for id in ["charlie", "alpha", "bravo"] {
            fleet.register(&spec(id));
        }
        let ids: Vec<String> = fleet.snapshot().into_iter().map(|s| s.id).collect();
        assert_eq!(ids, ["alpha", "bravo", "charlie"]);
        assert!(fleet.get("nobody").is_none());

        // Updating an unknown id must not synthesize an entry.
        fleet.running("nobody", Some(1));
        assert_eq!(fleet.snapshot().len(), 3);
    }

    #[test]
    fn log_ring_keeps_a_bounded_tail() {
        let ring = WorkerLogRing::new();
        for i in 0..(LOG_RING_CAPACITY + 50) {
            ring.push(&format!("line {i}"));
        }
        assert_eq!(ring.len(), LOG_RING_CAPACITY);
        assert!(!ring.is_empty());
        let tail = ring.tail(3);
        assert_eq!(
            tail,
            vec![
                format!("line {}", LOG_RING_CAPACITY + 47),
                format!("line {}", LOG_RING_CAPACITY + 48),
                format!("line {}", LOG_RING_CAPACITY + 49),
            ]
        );
        // Asking for more than the ring holds never exceeds the ring.
        assert_eq!(ring.tail(usize::MAX).len(), LOG_RING_CAPACITY);
    }

    #[test]
    fn log_ring_truncates_long_lines_with_a_marker() {
        let ring = WorkerLogRing::new();
        ring.push(&"x".repeat(LOG_LINE_MAX_BYTES * 3));
        let tail = ring.tail(1);
        assert_eq!(tail.len(), 1);
        assert!(tail[0].ends_with('…'), "missing truncation marker");
        assert!(tail[0].len() <= LOG_LINE_MAX_BYTES + '…'.len_utf8());
    }

    #[test]
    fn log_ring_ignores_blank_lines_and_trims_crlf() {
        let ring = WorkerLogRing::new();
        ring.push("");
        ring.push("   ");
        ring.push("hello\r");
        assert_eq!(ring.tail(10), vec!["hello".to_string()]);
    }

    /// Wait (bounded) for a fleet predicate; returns false on timeout.
    async fn wait_until(fleet: &FleetState, check: impl Fn(&WorkerSnapshot) -> bool) -> bool {
        for _ in 0..400 {
            if let Some(snap) = fleet.get("sleeper")
                && check(&snap)
            {
                return true;
            }
            tokio::time::sleep(Duration::from_millis(25)).await;
        }
        false
    }

    #[cfg(unix)]
    #[tokio::test]
    async fn restart_signal_respawns_the_child_and_drain_ends_the_run() {
        let drain = DrainController::new();
        let fleet = FleetState::new();
        let signal = Arc::new(tokio::sync::Notify::new());
        let spec = WorkerSpec::new("sleeper", "test", "sh")
            .args(["-c", "sleep 30"])
            .restart_signal(signal.clone());

        let mut supervisor = Supervisor::new(vec![spec]).fleet(fleet.clone());
        // Rate limit raised so the restart cannot trip it.
        supervisor = supervisor.rate_limit(10, Duration::from_secs(60));
        let run_drain = drain.clone();
        let run = tokio::spawn(async move { supervisor.run(run_drain).await });

        assert!(
            wait_until(&fleet, |s| s.phase == WorkerPhase::Running).await,
            "worker never reached running: {:?}",
            fleet.snapshot()
        );
        let pid_before = fleet.get("sleeper").and_then(|s| s.pid).expect("pid");

        signal.notify_one();
        assert!(
            wait_until(&fleet, |s| {
                s.restart_count == 1 && s.phase == WorkerPhase::Running
            })
            .await,
            "restart never completed: {:?}",
            fleet.snapshot()
        );
        let pid_after = fleet.get("sleeper").and_then(|s| s.pid).expect("pid");
        assert_ne!(
            pid_before, pid_after,
            "restart must spawn a fresh child, not keep the old one"
        );

        drain.begin_drain(ShutdownKind::Graceful);
        let results = tokio::time::timeout(Duration::from_secs(20), run)
            .await
            .expect("supervision did not end after drain")
            .expect("supervisor task panicked");
        assert_eq!(results.len(), 1);
        assert_eq!(results[0].restart_count, 1);
        assert_eq!(results[0].status, WorkerStatus::Stopped);
        assert_eq!(
            fleet.get("sleeper").map(|s| s.phase),
            Some(WorkerPhase::Stopped)
        );
    }
}
