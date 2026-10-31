//! Standalone MCP (Model Context Protocol) server for malkuth.
//!
//! Exposes the service-supervision toolkit — supervised child-process workers
//! and HTTP health/ready probes — as MCP tools over stdio, so an AI coding
//! assistant can run a fleet of processes, watch it, and act on it.
//!
//! The supervising arm (`malkuth_supervise`) keeps its historical semantics:
//! the *call* blocks for the lifetime of the supervision. Everything a second
//! caller needs while that call is outstanding is served by the non-blocking
//! arms — `status`, `drain`, `restart`, `logs` — which read and act on the
//! same in-process run registry. MCP requests are handled concurrently, so a
//! judge can observe and steer a run without waiting for it to finish.
//!
//! Activate with the `mcp` cargo feature and `malkuth mcp`.
//!
//! # Usage
//!
//! ```ignore
//! malkuth mcp
//! ```

#![cfg(feature = "mcp")]

use serde::Deserialize;
use serde_json::{Value, json};
use std::collections::HashMap;
use std::error::Error;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};

use rmcp::{
    ErrorData as McpError, RoleServer, ServerHandler, ServiceExt,
    handler::server::wrapper::Parameters, model::*, service::RequestContext, tool, tool_handler,
    tool_router,
};
use schemars::JsonSchema;

use crate::worker::{FleetState, WorkerLogRing};
use crate::{
    DrainController, RestartPolicy, ShutdownKind, Supervisor, WorkerInfo, WorkerPhase, WorkerSpec,
};

/// How many finished runs stay observable (their rings and final snapshots
/// remain readable after the blocking `supervise` call returns).
const MAX_RUNS: usize = 8;

/// Default budget for `drain` to wait for workers to report stopped.
const DEFAULT_DRAIN_TIMEOUT_SECS: u64 = 30;

/// `logs` defaults and hard caps — the tail is a *window*, and the caps are
/// what keeps a chatty worker from flooding a model's context.
const LOGS_DEFAULT_LINES: usize = 100;
const LOGS_MAX_LINES: usize = 500;
const LOGS_DEFAULT_BYTES: usize = 8 * 1024;
const LOGS_MAX_BYTES: usize = 64 * 1024;
const LOGS_MIN_BYTES: usize = 256;

static RUN_SEQ: AtomicU64 = AtomicU64::new(0);

/// One in-flight (or just-finished) supervision run.
///
/// The handle owns everything a controller needs to act on the run: the drain
/// controller, the per-worker restart signals, the log rings, and the live
/// fleet sink the supervisor publishes into.
struct RunHandle {
    run_id: String,
    started_at: SystemTime,
    fleet: FleetState,
    drain: DrainController,
    restart_signals: HashMap<String, Arc<tokio::sync::Notify>>,
    log_rings: HashMap<String, WorkerLogRing>,
    /// Workers whose restart was accepted, with the restart count observed
    /// at accept time: the entry is settled once the supervisor's count moves
    /// past it (the restart actually happened) or the worker ends.
    pending_restarts: Mutex<Vec<(String, u32)>>,
    finished: Mutex<Option<FinishedRun>>,
}

struct FinishedRun {
    workers: Vec<WorkerInfo>,
    finished_at: SystemTime,
}

impl RunHandle {
    fn is_finished(&self) -> bool {
        self.finished.lock().map(|f| f.is_some()).unwrap_or(false)
    }

    fn finished_workers(&self) -> Option<Vec<WorkerInfo>> {
        self.finished
            .lock()
            .ok()
            .and_then(|f| f.as_ref().map(|f| f.workers.clone()))
    }

    fn finished_unix_secs(&self) -> Option<u64> {
        self.finished
            .lock()
            .ok()
            .and_then(|f| f.as_ref().map(|f| unix_secs(f.finished_at)))
    }

    fn mark_finished(&self, workers: Vec<WorkerInfo>) {
        if let Ok(mut slot) = self.finished.lock() {
            *slot = Some(FinishedRun {
                workers,
                finished_at: SystemTime::now(),
            });
        }
    }

    /// Drop restart bookkeeping for requests the supervisor has already
    /// acted on: either the worker's restart count advanced past the value
    /// recorded at accept time, or the worker has ended.
    fn settle_pending_restarts(&self) {
        let Ok(mut pending) = self.pending_restarts.lock() else {
            return;
        };
        pending.retain(|(id, count_at_accept)| match self.fleet.get(id) {
            Some(snapshot) => {
                snapshot.phase != WorkerPhase::Stopped && snapshot.restart_count == *count_at_accept
            }
            None => false,
        });
    }

    fn snapshot_value(&self) -> Value {
        let elapsed = SystemTime::now()
            .duration_since(self.started_at)
            .map(|d| d.as_secs())
            .unwrap_or(0);
        let finished = self.finished_workers();
        let workers: Vec<Value> = match &finished {
            Some(workers) => workers
                .iter()
                .map(|w| {
                    json!({
                        "id": w.id,
                        "kind": w.kind,
                        "phase": "stopped",
                        "restart_policy": w.restart_policy,
                        "restart_count": w.restart_count,
                        "last_error": w.last_error,
                    })
                })
                .collect(),
            None => self
                .fleet
                .snapshot()
                .iter()
                .map(|s| serde_json::to_value(s).unwrap_or(Value::Null))
                .collect(),
        };
        json!({
            "run_id": self.run_id,
            "started_unix_secs": unix_secs(self.started_at),
            "elapsed_secs": elapsed,
            "finished": finished.is_some(),
            "finished_unix_secs": self.finished_unix_secs(),
            "draining": self.drain.is_draining(),
            "worker_count": workers.len(),
            "workers": workers,
        })
    }
}

/// Bounded page of log lines (the Spill side of log retrieval).
struct LogPage {
    text: String,
    lines: usize,
    bytes: usize,
    truncated: bool,
    omitted_lines: usize,
    omitted_bytes: usize,
}

/// Take the newest bytes of `lines`, prefixing a visible omission marker when
/// anything was dropped.
///
/// Two independent bounds apply: the caller's line window (older whole lines
/// are never read) and a byte cap (the head of the window is cut at a char
/// boundary). Both are reported, and the text carries a marker so a reader of
/// the text alone knows it is looking at a window.
fn page_logs(lines: Vec<String>, total_in_ring: usize, max_bytes: usize) -> LogPage {
    let mut text = lines.join("\n");
    let mut omitted_bytes = 0usize;
    if text.len() > max_bytes {
        let mut cut = text.len() - max_bytes;
        while cut < text.len() && !text.is_char_boundary(cut) {
            cut += 1;
        }
        omitted_bytes = cut;
        text = text.get(cut..).unwrap_or_default().to_string();
    }
    let omitted_lines = total_in_ring.saturating_sub(lines.len());
    let truncated = omitted_lines > 0 || omitted_bytes > 0;
    if truncated {
        let marker =
            format!("… [omitted {omitted_lines} earlier line(s), {omitted_bytes} byte(s)]\n");
        text.insert_str(0, &marker);
    }
    LogPage {
        bytes: text.len(),
        lines: lines.len(),
        text,
        truncated,
        omitted_lines,
        omitted_bytes,
    }
}

fn unix_secs(t: SystemTime) -> u64 {
    t.duration_since(UNIX_EPOCH)
        .map(|d| d.as_secs())
        .unwrap_or(0)
}

fn next_run_id() -> String {
    let seq = RUN_SEQ.fetch_add(1, Ordering::Relaxed) + 1;
    format!("run-{}-{seq}", unix_secs(SystemTime::now()))
}

/// Why a restart cannot be accepted for a worker in `phase`.
///
/// A draining worker is refused as well: the run-wide drain is already
/// ending it, so a restart would either be lost or turn the drain into a
/// respawn.
fn restart_blocker(phase: WorkerPhase, pending: bool) -> Option<&'static str> {
    if pending || phase == WorkerPhase::Restarting {
        return Some("already_restarting");
    }
    match phase {
        WorkerPhase::Stopped => Some("worker_stopped"),
        WorkerPhase::Draining => Some("worker_draining"),
        WorkerPhase::Starting | WorkerPhase::Running | WorkerPhase::Restarting => None,
    }
}

/// Reject worker lists that cannot be supervised unambiguously: ids address
/// workers in every control arm, so duplicates are unaddressable.
fn validate_worker_ids(ids: &[String]) -> Result<(), String> {
    let mut seen = std::collections::HashSet::new();
    for id in ids {
        let trimmed = id.trim();
        if trimmed.is_empty() {
            return Err("worker id must not be empty".to_string());
        }
        if !seen.insert(trimmed) {
            return Err(format!("duplicate worker id: {trimmed}"));
        }
    }
    Ok(())
}

struct Server {
    http: reqwest::Client,
    runs: Mutex<Vec<Arc<RunHandle>>>,
}

impl Server {
    fn tool_result(text: impl Into<String>) -> CallToolResult {
        CallToolResult::success(vec![Content::text(text)])
    }

    fn json_result(value: &Value) -> CallToolResult {
        Self::tool_result(serde_json::to_string_pretty(value).unwrap_or_default())
    }

    /// Uniform, machine-readable failure: every arm answers `ok`, and a
    /// failure carries a stable `code` a caller can branch on.
    fn error_payload(code: &str, message: impl Into<String>) -> Value {
        json!({"ok": false, "code": code, "message": message.into()})
    }

    fn find_run(&self, run_id: &str) -> Option<Arc<RunHandle>> {
        self.runs
            .lock()
            .ok()?
            .iter()
            .find(|r| r.run_id == run_id)
            .cloned()
    }

    /// Register a run, evicting the oldest finished one when the cap is hit.
    fn insert_run(&self, handle: Arc<RunHandle>) -> Result<(), &'static str> {
        let Ok(mut runs) = self.runs.lock() else {
            return Err("internal state poisoned");
        };
        if runs.iter().any(|r| r.run_id == handle.run_id) {
            return Err("run_id already in use");
        }
        if runs.len() >= MAX_RUNS {
            if let Some(pos) = runs.iter().position(|r| r.is_finished()) {
                runs.remove(pos);
            } else {
                return Err("too many concurrent runs");
            }
        }
        runs.push(handle);
        Ok(())
    }

    fn runs_snapshot(&self, only: Option<&str>) -> Result<Vec<Value>, Value> {
        let runs = self
            .runs
            .lock()
            .map_err(|_| Self::error_payload("internal_state_poisoned", "runs lock poisoned"))?;
        if let Some(run_id) = only {
            return match runs.iter().find(|r| r.run_id == run_id) {
                Some(run) => Ok(vec![run.snapshot_value()]),
                None => Err(Self::error_payload(
                    "unknown_run",
                    format!("no run with id `{run_id}`"),
                )),
            };
        }
        Ok(runs.iter().map(|r| r.snapshot_value()).collect())
    }

    async fn status_payload(&self, args: StatusArgs) -> Value {
        match self.runs_snapshot(args.run_id.as_deref()) {
            Ok(runs) => json!({"ok": true, "runs": runs}),
            Err(err) => err,
        }
    }

    /// `timeout_secs: Some(0)` returns immediately after signalling (the
    /// call is idempotent; a second drain on the same run reports
    /// `already: true`).
    async fn drain_payload(&self, args: DrainArgs) -> Value {
        let Some(run) = self.find_run(&args.run_id) else {
            return Self::error_payload("unknown_run", format!("no run with id `{}`", args.run_id));
        };
        let already = run.drain.is_draining();
        let kind = match args.kind.as_deref().map(str::trim) {
            Some(k) if k.eq_ignore_ascii_case("reload") => ShutdownKind::Reload,
            _ => ShutdownKind::Graceful,
        };
        run.drain.begin_drain(kind);
        let budget = Duration::from_secs(args.timeout_secs.unwrap_or(DEFAULT_DRAIN_TIMEOUT_SECS));
        let deadline = Instant::now() + budget;
        let mut timed_out = false;
        while !run.is_finished() {
            if Instant::now() >= deadline {
                timed_out = true;
                break;
            }
            tokio::time::sleep(Duration::from_millis(25)).await;
        }
        let workers: Vec<Value> = match run.finished_workers() {
            Some(workers) => workers
                .iter()
                .map(|w| {
                    json!({
                        "id": w.id,
                        "kind": w.kind,
                        "phase": "stopped",
                        "restart_count": w.restart_count,
                        "last_error": w.last_error,
                    })
                })
                .collect(),
            None => run
                .fleet
                .snapshot()
                .iter()
                .map(|s| serde_json::to_value(s).unwrap_or(Value::Null))
                .collect(),
        };
        json!({
            "ok": true,
            "run_id": run.run_id,
            "accepted": true,
            "already": already,
            "draining": run.drain.is_draining(),
            "finished": run.is_finished(),
            "timed_out": timed_out,
            "workers": workers,
        })
    }

    async fn restart_payload(&self, args: RestartArgs) -> Value {
        let Some(run) = self.find_run(&args.run_id) else {
            return Self::error_payload("unknown_run", format!("no run with id `{}`", args.run_id));
        };
        if run.is_finished() {
            return Self::error_payload(
                "run_finished",
                format!("run `{}` has already finished", args.run_id),
            );
        }
        let Some(signal) = run.restart_signals.get(&args.worker_id).cloned() else {
            return Self::error_payload(
                "unknown_worker",
                format!(
                    "run `{}` has no worker id `{}`",
                    args.run_id, args.worker_id
                ),
            );
        };
        run.settle_pending_restarts();
        let phase = run
            .fleet
            .get(&args.worker_id)
            .map(|s| s.phase)
            .unwrap_or(WorkerPhase::Stopped);
        let pending = run
            .pending_restarts
            .lock()
            .map(|p| p.iter().any(|(id, _)| id == &args.worker_id))
            .unwrap_or(false);
        if let Some(code) = restart_blocker(phase, pending) {
            return Self::error_payload(
                code,
                format!(
                    "worker `{}` cannot restart now (phase {})",
                    args.worker_id,
                    serde_json::to_value(phase)
                        .ok()
                        .and_then(|v| v.as_str().map(str::to_string))
                        .unwrap_or_else(|| "unknown".to_string())
                ),
            );
        }
        let restart_count_now = run
            .fleet
            .get(&args.worker_id)
            .map(|s| s.restart_count)
            .unwrap_or(0);
        if let Ok(mut p) = run.pending_restarts.lock() {
            p.push((args.worker_id.clone(), restart_count_now));
        }
        signal.notify_one();
        json!({
            "ok": true,
            "run_id": args.run_id,
            "worker_id": args.worker_id,
            "accepted": true,
            "phase": "restarting",
        })
    }

    async fn logs_payload(&self, args: LogsArgs) -> Value {
        let Some(run) = self.find_run(&args.run_id) else {
            return Self::error_payload("unknown_run", format!("no run with id `{}`", args.run_id));
        };
        let worker_id = match args.worker_id.as_deref().map(str::trim) {
            Some(id) if !id.is_empty() => id.to_string(),
            _ => {
                if run.log_rings.len() == 1 {
                    run.log_rings.keys().next().cloned().unwrap_or_default()
                } else {
                    return Self::error_payload(
                        "worker_required",
                        format!(
                            "run `{}` has {} workers — pass `worker_id`",
                            args.run_id,
                            run.log_rings.len()
                        ),
                    );
                }
            }
        };
        let Some(ring) = run.log_rings.get(&worker_id) else {
            return Self::error_payload(
                "unknown_worker",
                format!("run `{}` has no worker id `{}`", args.run_id, worker_id),
            );
        };
        let max_lines = args
            .lines
            .map(|n| n as usize)
            .unwrap_or(LOGS_DEFAULT_LINES)
            .clamp(1, LOGS_MAX_LINES);
        let max_bytes = args
            .max_bytes
            .map(|n| n as usize)
            .unwrap_or(LOGS_DEFAULT_BYTES)
            .clamp(LOGS_MIN_BYTES, LOGS_MAX_BYTES);
        let total = ring.len();
        let page = page_logs(ring.tail(max_lines), total, max_bytes);
        json!({
            "ok": true,
            "run_id": run.run_id,
            "worker_id": worker_id,
            "total_lines_in_ring": total,
            "lines_returned": page.lines,
            "bytes": page.bytes,
            "truncated": page.truncated,
            "omitted_lines": page.omitted_lines,
            "omitted_bytes": page.omitted_bytes,
            "text": page.text,
        })
    }
}

// ── Tool argument structs ────────────────────────────

#[derive(Debug, Deserialize, JsonSchema)]
struct WorkerSpecArg {
    /// Unique id for this worker (used in status reports).
    id: String,
    /// Logical kind label (informational).
    kind: String,
    /// Program to run (resolved via $PATH; absolute paths work too).
    program: String,
    /// Command-line arguments.
    #[serde(default)]
    args: Vec<String>,
    /// Extra env vars as "KEY=VALUE" strings.
    #[serde(default)]
    env: Vec<String>,
    /// Restart policy: permanent (default), transient, temporary.
    restart_policy: Option<String>,
}

#[derive(Debug, Deserialize, JsonSchema)]
struct SuperviseArgs {
    /// The workers to launch and supervise concurrently.
    workers: Vec<WorkerSpecArg>,
    /// Max restarts allowed within `window_secs` before cooldown trips.
    #[serde(default)]
    max_restarts: Option<u32>,
    /// Sliding-window length for restart rate-limiting (seconds, default 60).
    #[serde(default)]
    window_secs: Option<u64>,
    /// Cooldown after the rate limit trips (seconds, default 30).
    #[serde(default)]
    cooldown_secs: Option<u64>,
    /// Hard cap on how long the supervisor runs before force-draining
    /// (seconds). Omit to run until every worker exits on its own.
    #[serde(default)]
    timeout_secs: Option<u64>,
    /// Caller-chosen run id, addressable by status/drain/restart/logs.
    /// Omit for a generated one (returned in the result).
    #[serde(default)]
    run_id: Option<String>,
}

#[derive(Debug, Deserialize, JsonSchema)]
struct StatusArgs {
    /// Restrict the snapshot to one run id (omit for every known run).
    #[serde(default)]
    run_id: Option<String>,
}

#[derive(Debug, Deserialize, JsonSchema)]
struct RestartArgs {
    /// Run id returned by malkuth_supervise.
    run_id: String,
    /// Worker id to restart (must exist in that run).
    worker_id: String,
}

#[derive(Debug, Deserialize, JsonSchema)]
struct DrainArgs {
    /// Run id returned by malkuth_supervise.
    run_id: String,
    /// Drain kind: graceful (default) or reload.
    #[serde(default)]
    kind: Option<String>,
    /// How long to wait for workers to report stopped (seconds, default 30;
    /// 0 returns immediately after signalling).
    #[serde(default)]
    timeout_secs: Option<u64>,
}

#[derive(Debug, Deserialize, JsonSchema)]
struct LogsArgs {
    /// Run id returned by malkuth_supervise.
    run_id: String,
    /// Worker id whose tail to read. May be omitted when the run has exactly
    /// one worker.
    #[serde(default)]
    worker_id: Option<String>,
    /// Number of trailing lines to read (default 100, capped at 500).
    #[serde(default)]
    lines: Option<u32>,
    /// Byte budget for the returned text (default 8192, capped at 65536).
    /// Older bytes are cut at a char boundary and counted in `omitted_bytes`.
    #[serde(default)]
    max_bytes: Option<u32>,
}

#[derive(Debug, Deserialize, JsonSchema)]
struct ProbeArgs {
    /// Base URL of the service to probe (e.g. "http://localhost:8080").
    url: String,
    /// Which probe to hit: healthz (default), readyz, or both.
    #[serde(default)]
    kind: Option<String>,
    /// Per-request timeout in seconds (default 5).
    #[serde(default)]
    timeout_secs: Option<u64>,
}

// ── Supervision + control tools ──────────────────────

#[tool_router]
impl Server {
    #[tool(
        description = "Launch a set of child-process workers under malkuth's supervisor and run until they all exit (or the timeout elapses). Each worker is restarted per its policy (permanent/transient/temporary) with a sliding-window rate limit to prevent crash storms. Returns the final status snapshot of every worker. This call blocks for the lifetime of the supervision; while it is outstanding, use malkuth_status / malkuth_logs / malkuth_restart / malkuth_drain on the returned run_id to observe and steer it (those calls are non-blocking and safe to issue concurrently)."
    )]
    async fn malkuth_supervise(
        &self,
        Parameters(args): Parameters<SuperviseArgs>,
        _context: RequestContext<RoleServer>,
    ) -> Result<CallToolResult, McpError> {
        let ids: Vec<String> = args.workers.iter().map(|w| w.id.clone()).collect();
        if ids.is_empty() {
            return Err(McpError::invalid_params(
                "at least one worker is required",
                None,
            ));
        }
        if let Err(e) = validate_worker_ids(&ids) {
            return Err(McpError::invalid_params(e, None));
        }

        let run_id = args.run_id.unwrap_or_else(next_run_id);
        let mut restart_signals = HashMap::new();
        let mut log_rings = HashMap::new();
        let mut specs = Vec::with_capacity(args.workers.len());
        for w in args.workers {
            let id = w.id.trim().to_string();
            let signal = Arc::new(tokio::sync::Notify::new());
            let ring = WorkerLogRing::new();
            restart_signals.insert(id.clone(), signal.clone());
            log_rings.insert(id.clone(), ring.clone());
            let mut spec = WorkerSpec::new(id, w.kind, w.program)
                .args(w.args)
                .policy(parse_policy(w.restart_policy.as_deref()))
                .restart_signal(signal)
                .capture_logs(ring);
            for pair in w.env {
                if let Some((k, v)) = pair.split_once('=') {
                    spec = spec.env(k.trim(), v);
                }
            }
            specs.push(spec);
        }

        let drain = DrainController::new();
        let fleet = FleetState::new();
        let handle = Arc::new(RunHandle {
            run_id: run_id.clone(),
            started_at: SystemTime::now(),
            fleet: fleet.clone(),
            drain: drain.clone(),
            restart_signals,
            log_rings,
            pending_restarts: Mutex::new(Vec::new()),
            finished: Mutex::new(None),
        });
        if let Err(e) = self.insert_run(handle.clone()) {
            return Err(McpError::invalid_params(e, None));
        }

        let mut supervisor = Supervisor::new(specs).fleet(fleet);
        if let (Some(max), Some(win)) = (args.max_restarts, args.window_secs) {
            supervisor = supervisor.rate_limit(max, Duration::from_secs(win));
        }
        if let Some(cd) = args.cooldown_secs {
            supervisor = supervisor.cooldown(Duration::from_secs(cd));
        }

        let run = supervisor.run(drain.clone());

        // If a timeout is set, force-drain after it so the call always returns.
        let result = if let Some(secs) = args.timeout_secs {
            let timeout = Duration::from_secs(secs);
            match tokio::time::timeout(timeout, run).await {
                Ok(final_info) => final_info,
                Err(_) => {
                    drain.begin_drain(ShutdownKind::Graceful);
                    // The supervision future is dropped here: kill_on_drop
                    // terminates the children, so the run is over even though
                    // there is no final snapshot to report.
                    Vec::new()
                }
            }
        } else {
            run.await
        };

        handle.mark_finished(result.clone());
        let summary = json!({
            "run_id": run_id,
            "timed_out": args.timeout_secs.is_some() && result.is_empty(),
            "workers": result,
        });
        Ok(Self::json_result(&summary))
    }

    #[tool(
        description = "Non-blocking snapshot of every supervision run this MCP server knows about (or one run when `run_id` is passed). Per worker: id, kind, phase (starting/running/restarting/draining/stopped), pid, uptime_secs, restart_policy, restart_count, last_error. Safe to call while malkuth_supervise is still blocking. Answer shape: {\"ok\":true,\"runs\":[...]}, or {\"ok\":false,\"code\":\"unknown_run\",...}. Read-only: no side effects."
    )]
    async fn malkuth_status(
        &self,
        Parameters(args): Parameters<StatusArgs>,
        _context: RequestContext<RoleServer>,
    ) -> Result<CallToolResult, McpError> {
        Ok(Self::json_result(&self.status_payload(args).await))
    }

    #[tool(
        description = "Restart ONE worker of a running supervision, addressed by run_id + worker_id. Side effect: the current child receives a graceful stop (SIGTERM/CTRL_BREAK, escalating to a kill after 30s) and a fresh child is spawned under the same id; the worker's restart budget/window still applies. Idempotency: a second restart for a worker already restarting is refused, never queued. Failure shapes are stable codes: unknown_run, unknown_worker, run_finished, worker_stopped, worker_draining, already_restarting. Success: {\"ok\":true,\"accepted\":true,\"phase\":\"restarting\"}."
    )]
    async fn malkuth_restart(
        &self,
        Parameters(args): Parameters<RestartArgs>,
        _context: RequestContext<RoleServer>,
    ) -> Result<CallToolResult, McpError> {
        Ok(Self::json_result(&self.restart_payload(args).await))
    }

    #[tool(
        description = "Gracefully drain a running supervision (optionally a reload drain) and wait up to `timeout_secs` for workers to stop. Side effect: every worker gets SIGTERM/CTRL_BREAK and up to 30s to finish in-flight work before a kill; the run then ends and the blocking malkuth_supervise call returns its final snapshot. Idempotent: calling drain twice is safe — the second answer reports \"already\": true. Answer includes finished/timed_out plus the per-worker snapshot."
    )]
    async fn malkuth_drain(
        &self,
        Parameters(args): Parameters<DrainArgs>,
        _context: RequestContext<RoleServer>,
    ) -> Result<CallToolResult, McpError> {
        Ok(Self::json_result(&self.drain_payload(args).await))
    }

    #[tool(
        description = "Read a BOUNDED tail of one worker's captured stdout/stderr (capture is on for MCP-supervised workers only). `lines` defaults to 100 and is capped at 500; `max_bytes` defaults to 8192 and is capped at 65536. Anything dropped is reported two ways: metadata (truncated, omitted_lines, omitted_bytes, total_lines_in_ring) and a leading `… [omitted …]` marker inside text — so a caller can tell a window from the whole history and re-read with a larger budget if it matters. Failure codes: unknown_run, unknown_worker, worker_required (run has several workers and none was named)."
    )]
    async fn malkuth_logs(
        &self,
        Parameters(args): Parameters<LogsArgs>,
        _context: RequestContext<RoleServer>,
    ) -> Result<CallToolResult, McpError> {
        Ok(Self::json_result(&self.logs_payload(args).await))
    }

    #[tool(
        description = "Probe a service's healthz / readyz endpoints (the convention malkuth's probes feature serves). Returns each endpoint's HTTP status. Useful to confirm a supervised service actually came up."
    )]
    async fn malkuth_probe(
        &self,
        Parameters(args): Parameters<ProbeArgs>,
        _context: RequestContext<RoleServer>,
    ) -> Result<CallToolResult, McpError> {
        let timeout = Duration::from_secs(args.timeout_secs.unwrap_or(5));
        let base = args.url.trim_end_matches('/');
        let kind = args.kind.as_deref().unwrap_or("healthz");
        let endpoints: Vec<&str> = match kind {
            "readyz" | "ready" => vec!["/readyz"],
            "both" => vec!["/healthz", "/readyz"],
            _ => vec!["/healthz"],
        };

        let mut results = Vec::new();
        for ep in endpoints {
            let url = format!("{base}{ep}");
            let status = match self.http.get(&url).timeout(timeout).send().await {
                Ok(r) => r.status().as_u16(),
                Err(e) => {
                    results.push(json!({"url": url, "ok": false, "error": e.to_string()}));
                    continue;
                }
            };
            results.push(json!({"url": url, "ok": (200..300).contains(&status), "status": status}));
        }
        Ok(Self::json_result(&Value::Array(results)))
    }
}

// ── ServerHandler ────────────────────────────────────

#[tool_handler(router = Server::tool_router())]
impl ServerHandler for Server {}

// ── helpers ──────────────────────────────────────────

fn parse_policy(name: Option<&str>) -> RestartPolicy {
    let Some(name) = name.map(str::trim).filter(|s| !s.is_empty()) else {
        return RestartPolicy::Permanent;
    };
    match name.to_ascii_lowercase().as_str() {
        "transient" => RestartPolicy::Transient,
        "temporary" => RestartPolicy::Temporary,
        _ => RestartPolicy::Permanent,
    }
}

// ── proxy detection ──────────────────────────────────

/// Detect HTTP(S) proxy from environment variables.
///
/// Checks env vars in priority order:
/// 1. `MALKUTH_PROXY` (malkuth-specific override)
/// 2. `HTTP_PROXY`
/// 3. `HTTPS_PROXY`
/// 4. `ALL_PROXY`
fn detect_proxy() -> Option<reqwest::Proxy> {
    let candidates: [&str; 4] = ["MALKUTH_PROXY", "HTTP_PROXY", "HTTPS_PROXY", "ALL_PROXY"];
    for var in candidates {
        if let Ok(val) = std::env::var(var) {
            let trimmed = val.trim();
            if !trimmed.is_empty() {
                if let Ok(proxy) = reqwest::Proxy::all(trimmed) {
                    return Some(proxy);
                }
            }
        }
    }
    None
}

// ── public entry point ───────────────────────────────

pub async fn run() -> Result<(), Box<dyn Error>> {
    let mut builder = crate::http_client_builder().timeout(Duration::from_secs(10));
    if let Some(proxy) = detect_proxy() {
        builder = builder.proxy(proxy);
    }
    let server = Server {
        http: builder.build().unwrap_or_default(),
        runs: Mutex::new(Vec::new()),
    };
    let transport = rmcp::transport::stdio();
    let server_handle = server.serve(transport).await?;
    server_handle.waiting().await?;
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    fn test_server() -> Server {
        Server {
            http: crate::http_client_builder().build().unwrap_or_default(),
            runs: Mutex::new(Vec::new()),
        }
    }

    fn run_with_workers(run_id: &str, workers: &[(&str, &str)]) -> Arc<RunHandle> {
        let fleet = FleetState::new();
        let mut restart_signals = HashMap::new();
        let mut log_rings = HashMap::new();
        for (id, kind) in workers {
            let spec = WorkerSpec::new(*id, *kind, "/bin/true");
            fleet.register(&spec);
            fleet.running(id, Some(4242));
            restart_signals.insert(id.to_string(), Arc::new(tokio::sync::Notify::new()));
            log_rings.insert(id.to_string(), WorkerLogRing::new());
        }
        Arc::new(RunHandle {
            run_id: run_id.to_string(),
            started_at: SystemTime::now(),
            fleet,
            drain: DrainController::new(),
            restart_signals,
            log_rings,
            pending_restarts: Mutex::new(Vec::new()),
            finished: Mutex::new(None),
        })
    }

    #[tokio::test]
    async fn status_reports_every_worker_field() {
        let server = test_server();
        let handle = run_with_workers("r1", &[("beta", "backend"), ("alpha", "worker")]);
        server.insert_run(handle).unwrap();

        let payload = server.status_payload(StatusArgs { run_id: None }).await;
        assert_eq!(payload["ok"], json!(true));
        let runs = payload["runs"].as_array().unwrap();
        assert_eq!(runs.len(), 1);
        let run = &runs[0];
        assert_eq!(run["run_id"], json!("r1"));
        assert_eq!(run["finished"], json!(false));
        assert_eq!(run["draining"], json!(false));
        assert_eq!(run["worker_count"], json!(2));

        let workers = run["workers"].as_array().unwrap();
        // Sorted by id, and every documented field is present.
        assert_eq!(workers[0]["id"], json!("alpha"));
        assert_eq!(workers[1]["id"], json!("beta"));
        for key in [
            "id",
            "kind",
            "phase",
            "pid",
            "uptime_secs",
            "restart_policy",
            "restart_count",
            "last_error",
        ] {
            assert!(
                workers[0].get(key).is_some(),
                "status worker snapshot is missing `{key}`: {workers:?}"
            );
        }
        assert_eq!(workers[0]["phase"], json!("running"));
        assert_eq!(workers[0]["pid"], json!(4242));
    }

    #[tokio::test]
    async fn status_unknown_run_is_a_typed_failure() {
        let server = test_server();
        let payload = server
            .status_payload(StatusArgs {
                run_id: Some("nope".into()),
            })
            .await;
        assert_eq!(payload["ok"], json!(false));
        assert_eq!(payload["code"], json!("unknown_run"));
    }

    #[tokio::test]
    async fn drain_is_idempotent_and_reports_already() {
        let server = test_server();
        server
            .insert_run(run_with_workers("r2", &[("only", "svc")]))
            .unwrap();
        let first = server
            .drain_payload(DrainArgs {
                run_id: "r2".into(),
                kind: None,
                timeout_secs: Some(0),
            })
            .await;
        assert_eq!(first["ok"], json!(true));
        assert_eq!(first["accepted"], json!(true));
        assert_eq!(first["already"], json!(false));
        assert_eq!(first["draining"], json!(true));

        let second = server
            .drain_payload(DrainArgs {
                run_id: "r2".into(),
                kind: Some("reload".into()),
                timeout_secs: Some(0),
            })
            .await;
        assert_eq!(second["ok"], json!(true));
        assert_eq!(second["already"], json!(true));
    }

    #[tokio::test]
    async fn drain_unknown_run_is_a_typed_failure() {
        let server = test_server();
        let payload = server
            .drain_payload(DrainArgs {
                run_id: "nope".into(),
                kind: None,
                timeout_secs: Some(0),
            })
            .await;
        assert_eq!(payload["code"], json!("unknown_run"));
    }

    #[tokio::test]
    async fn restart_accepts_running_worker_and_refuses_second_call() {
        let server = test_server();
        server
            .insert_run(run_with_workers("r3", &[("api", "backend")]))
            .unwrap();
        let first = server
            .restart_payload(RestartArgs {
                run_id: "r3".into(),
                worker_id: "api".into(),
            })
            .await;
        assert_eq!(first["ok"], json!(true));
        assert_eq!(first["accepted"], json!(true));
        assert_eq!(first["phase"], json!("restarting"));

        let second = server
            .restart_payload(RestartArgs {
                run_id: "r3".into(),
                worker_id: "api".into(),
            })
            .await;
        assert_eq!(second["ok"], json!(false));
        assert_eq!(second["code"], json!("already_restarting"));
    }

    #[tokio::test]
    async fn restart_failure_shapes_are_distinct() {
        let server = test_server();
        server
            .insert_run(run_with_workers("r4", &[("api", "backend")]))
            .unwrap();

        let unknown_run = server
            .restart_payload(RestartArgs {
                run_id: "missing".into(),
                worker_id: "api".into(),
            })
            .await;
        assert_eq!(unknown_run["code"], json!("unknown_run"));

        let unknown_worker = server
            .restart_payload(RestartArgs {
                run_id: "r4".into(),
                worker_id: "ghost".into(),
            })
            .await;
        assert_eq!(unknown_worker["code"], json!("unknown_worker"));

        // A finished run can no longer be steered.
        server.runs.lock().unwrap()[0].mark_finished(Vec::new());
        let finished = server
            .restart_payload(RestartArgs {
                run_id: "r4".into(),
                worker_id: "api".into(),
            })
            .await;
        assert_eq!(finished["code"], json!("run_finished"));
    }

    #[test]
    fn restart_blocker_covers_every_phase() {
        assert_eq!(restart_blocker(WorkerPhase::Running, false), None);
        assert_eq!(restart_blocker(WorkerPhase::Starting, false), None);
        assert_eq!(
            restart_blocker(WorkerPhase::Draining, false),
            Some("worker_draining")
        );
        assert_eq!(
            restart_blocker(WorkerPhase::Restarting, false),
            Some("already_restarting")
        );
        assert_eq!(
            restart_blocker(WorkerPhase::Running, true),
            Some("already_restarting")
        );
        assert_eq!(
            restart_blocker(WorkerPhase::Stopped, false),
            Some("worker_stopped")
        );
    }

    #[tokio::test]
    async fn restart_refuses_a_worker_that_already_exited() {
        let server = test_server();
        let handle = run_with_workers("r5", &[("one", "svc")]);
        handle
            .fleet
            .finish("one", WorkerPhase::Stopped, 0, Some("exit 1".into()));
        server.insert_run(handle).unwrap();
        let payload = server
            .restart_payload(RestartArgs {
                run_id: "r5".into(),
                worker_id: "one".into(),
            })
            .await;
        assert_eq!(payload["code"], json!("worker_stopped"));
    }

    #[tokio::test]
    async fn logs_are_bounded_and_marked() {
        let server = test_server();
        let handle = run_with_workers("r6", &[("chatty", "svc")]);
        let ring = handle.log_rings.get("chatty").unwrap().clone();
        for i in 0..10 {
            ring.push(&format!("line {i}"));
        }
        server.insert_run(handle).unwrap();

        let payload = server
            .logs_payload(LogsArgs {
                run_id: "r6".into(),
                worker_id: Some("chatty".into()),
                lines: Some(3),
                max_bytes: None,
            })
            .await;
        assert_eq!(payload["ok"], json!(true));
        assert_eq!(payload["total_lines_in_ring"], json!(10));
        assert_eq!(payload["lines_returned"], json!(3));
        assert_eq!(payload["truncated"], json!(true));
        assert_eq!(payload["omitted_lines"], json!(7));
        let text = payload["text"].as_str().unwrap();
        assert!(text.ends_with("line 9"), "unexpected tail: {text}");
        assert!(
            text.starts_with("… [omitted 7 earlier line(s)"),
            "missing truncation marker: {text}"
        );
    }

    #[tokio::test]
    async fn logs_byte_cap_cuts_the_head_and_counts_it() {
        let server = test_server();
        let handle = run_with_workers("r7", &[("loud", "svc")]);
        let ring = handle.log_rings.get("loud").unwrap().clone();
        for _ in 0..20 {
            ring.push(&"x".repeat(200));
        }
        server.insert_run(handle).unwrap();
        let payload = server
            .logs_payload(LogsArgs {
                run_id: "r7".into(),
                worker_id: Some("loud".into()),
                lines: Some(20),
                max_bytes: Some(LOGS_MIN_BYTES as u32),
            })
            .await;
        assert_eq!(payload["ok"], json!(true));
        assert_eq!(payload["truncated"], json!(true));
        assert!(
            payload["omitted_bytes"].as_u64().unwrap() > 0,
            "byte truncation was not reported: {payload}"
        );
        let bytes = payload["bytes"].as_u64().unwrap() as usize;
        assert!(
            bytes <= LOGS_MIN_BYTES + 64,
            "page exceeded its byte budget: {bytes}"
        );
    }

    #[tokio::test]
    async fn logs_require_a_worker_when_the_run_has_several() {
        let server = test_server();
        server
            .insert_run(run_with_workers("r8", &[("a", "svc"), ("b", "svc")]))
            .unwrap();
        let ambiguous = server
            .logs_payload(LogsArgs {
                run_id: "r8".into(),
                worker_id: None,
                lines: None,
                max_bytes: None,
            })
            .await;
        assert_eq!(ambiguous["code"], json!("worker_required"));

        let unknown = server
            .logs_payload(LogsArgs {
                run_id: "r8".into(),
                worker_id: Some("c".into()),
                lines: None,
                max_bytes: None,
            })
            .await;
        assert_eq!(unknown["code"], json!("unknown_worker"));
    }

    #[tokio::test]
    async fn logs_default_to_the_single_worker() {
        let server = test_server();
        let handle = run_with_workers("r9", &[("solo", "svc")]);
        handle.log_rings.get("solo").unwrap().push("hello");
        server.insert_run(handle).unwrap();
        let payload = server
            .logs_payload(LogsArgs {
                run_id: "r9".into(),
                worker_id: None,
                lines: None,
                max_bytes: None,
            })
            .await;
        assert_eq!(payload["worker_id"], json!("solo"));
        assert_eq!(payload["text"], json!("hello"));
        assert_eq!(payload["truncated"], json!(false));
    }

    #[test]
    fn page_logs_never_splits_a_multibyte_character() {
        let lines = vec!["日志行".repeat(50), "last".to_string()];
        let page = page_logs(lines, 2, LOGS_MIN_BYTES);
        assert!(page.truncated);
        // The cut respects UTF-8 boundaries, so the page is valid UTF-8 text.
        assert!(std::str::from_utf8(page.text.as_bytes()).is_ok());
        assert!(page.text.ends_with("last"));
    }

    #[test]
    fn worker_ids_must_be_unique_and_non_empty() {
        assert!(validate_worker_ids(&["a".into(), "b".into()]).is_ok());
        assert!(validate_worker_ids(&["a".into(), "a".into()]).is_err());
        assert!(validate_worker_ids(&["  ".into()]).is_err());
    }

    #[tokio::test]
    async fn run_cap_evicts_only_finished_runs() {
        let server = test_server();
        for i in 0..MAX_RUNS {
            server
                .insert_run(run_with_workers(&format!("live-{i}"), &[("w", "svc")]))
                .unwrap();
        }
        // All runs are live: a new one must be refused, not silently dropped.
        let refused = server.insert_run(run_with_workers("overflow", &[("w", "svc")]));
        assert_eq!(refused, Err("too many concurrent runs"));

        // Once one finishes it is the eviction candidate.
        server.runs.lock().unwrap()[0].mark_finished(Vec::new());
        server
            .insert_run(run_with_workers("overflow", &[("w", "svc")]))
            .unwrap();
        assert!(server.find_run("live-0").is_none());
        assert!(server.find_run("overflow").is_some());
    }

    #[tokio::test]
    async fn duplicate_run_id_is_refused() {
        let server = test_server();
        server
            .insert_run(run_with_workers("same", &[("w", "svc")]))
            .unwrap();
        let dup = server.insert_run(run_with_workers("same", &[("w", "svc")]));
        assert_eq!(dup, Err("run_id already in use"));
    }
}
