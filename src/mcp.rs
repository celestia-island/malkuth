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

/// How many runs stay in the registry. When it is full, only a *finished*
/// run may be evicted (its handle, log rings and final snapshot go with it);
/// with every slot live, a new run is refused rather than silently dropping
/// observability of a run still in flight.
const MAX_RUNS: usize = 8;

/// Retired run ids remembered so a caller cannot recycle a name that already
/// addressed a (now evicted) run. Bounded: ids older than this many insertions
/// become reusable, which is documented on the tool.
const MAX_REMEMBERED_RUN_IDS: usize = 256;

/// Default budget for `drain` to wait for workers to report stopped.
const DEFAULT_DRAIN_TIMEOUT_SECS: u64 = 30;

/// Operator-requested restarts admitted per worker per window. This is a
/// *gate on the control arm* (checked synchronously, refusing the call), not
/// the supervisor's crash budget: a manual restart must never consume the
/// allowance that exists to damp crash loops.
const EXPLICIT_RESTART_MAX: usize = 5;
const EXPLICIT_RESTART_WINDOW: Duration = Duration::from_secs(60);

/// `logs` defaults and hard caps — the tail is a *window*, and the caps are
/// what keeps a chatty worker from flooding a model's context. The pins in
/// `log_caps_are_contract_pinned` make widening them a deliberate, visible
/// change rather than a silent widening of the context budget.
const LOGS_DEFAULT_LINES: usize = 100;
const LOGS_MAX_LINES: usize = 500;
const LOGS_DEFAULT_BYTES: usize = 8 * 1024;
const LOGS_MAX_BYTES: usize = 64 * 1024;
const LOGS_MIN_BYTES: usize = 256;

/// Bytes kept free for the omission marker so `bytes <= max_bytes` holds
/// exactly, marker included.
const LOG_MARKER_RESERVE: usize = 64;

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
    restart_book: Mutex<RestartBook>,
    finished: Mutex<Option<FinishedRun>>,
}

/// Restart bookkeeping for one run.
///
/// `pending` is (worker id, worker restart count observed at accept time): an
/// entry is settled once the supervisor's count moves past it (the restart
/// happened) or the worker ends. `stamps` is the operator's restart budget
/// window. Both live under ONE mutex, so "settle → check pending → check
/// budget → record" is a single critical section: two concurrent calls cannot
/// both be accepted.
#[derive(Default)]
struct RestartBook {
    pending: Vec<(String, u32)>,
    stamps: Vec<(String, Instant)>,
}

struct FinishedRun {
    #[allow(dead_code)]
    // Kept as the record anchor: the per-worker truth is the fleet snapshot,
    // the terminal `WorkerInfo` list is what the blocking call returned.
    workers: Vec<WorkerInfo>,
    finished_at: SystemTime,
}

impl RunHandle {
    fn is_finished(&self) -> bool {
        self.finished.lock().map(|f| f.is_some()).unwrap_or(false)
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

    /// Decide — and record — one operator restart request atomically.
    ///
    /// The fleet is snapshotted BEFORE the book lock is taken, so the two
    /// mutexes are never nested (no lock-order inversion). Returns the stable
    /// failure code when the request cannot be accepted.
    fn accept_restart(&self, worker_id: &str) -> Result<(), &'static str> {
        let phases: HashMap<String, (WorkerPhase, u32)> = self
            .fleet
            .snapshot()
            .into_iter()
            .map(|s| (s.id, (s.phase, s.restart_count)))
            .collect();
        let (phase, count) = phases
            .get(worker_id)
            .copied()
            .unwrap_or((WorkerPhase::Stopped, 0));

        let Ok(mut book) = self.restart_book.lock() else {
            return Err("internal_state_poisoned");
        };
        book.pending.retain(|(id, count_at_accept)| {
            match phases.get(id) {
                // Settled: the worker ended, or the supervisor's counter moved
                // past the value recorded when the request was accepted.
                Some((WorkerPhase::Stopped, _)) | None => false,
                Some((_, current)) => current == count_at_accept,
            }
        });
        book.stamps
            .retain(|(_, at)| at.elapsed() < EXPLICIT_RESTART_WINDOW);

        let pending_same = book.pending.iter().any(|(id, _)| id == worker_id);
        if let Some(code) = restart_blocker(phase, pending_same) {
            return Err(code);
        }
        let in_window = book.stamps.iter().filter(|(id, _)| id == worker_id).count();
        if in_window >= EXPLICIT_RESTART_MAX {
            return Err("restart_budget_exhausted");
        }
        book.pending.push((worker_id.to_string(), count));
        book.stamps.push((worker_id.to_string(), Instant::now()));
        Ok(())
    }

    /// The run snapshot served by `status` and `drain`.
    ///
    /// Every worker is serialized from the live fleet sink — including a
    /// finished run, where the sink holds the terminal `stopped` phase — so
    /// `pid`, `uptime_secs` and `restart_policy` are always present (`null`
    /// when they do not apply) instead of vanishing from the payload.
    fn snapshot_value(&self) -> Value {
        let elapsed = SystemTime::now()
            .duration_since(self.started_at)
            .map(|d| d.as_secs())
            .unwrap_or(0);
        let workers: Vec<Value> = self
            .fleet
            .snapshot()
            .iter()
            .map(|s| serde_json::to_value(s).unwrap_or(Value::Null))
            .collect();
        json!({
            "run_id": self.run_id,
            "started_unix_secs": unix_secs(self.started_at),
            "elapsed_secs": elapsed,
            "finished": self.is_finished(),
            "finished_unix_secs": self.finished_unix_secs(),
            "draining": self.drain.is_draining(),
            "drain_kind": self.drain.kind().map(shutdown_kind_name),
            "worker_count": workers.len(),
            "workers": workers,
        })
    }
}

/// Bounded page of log lines (the Spill side of log retrieval).
///
/// Counts are *derived from the final text*, not from the request: after the
/// line window and the byte budget have both been applied,
/// `lines_returned + omitted_lines == total_lines_in_ring` and
/// `bytes <= max_bytes` hold exactly (marker included).
struct LogPage {
    text: String,
    lines: usize,
    bytes: usize,
    truncated: bool,
    omitted_lines: usize,
    omitted_bytes: usize,
}

/// Page `all` (the whole ring, oldest first) down to at most `max_lines` lines
/// and `max_bytes` bytes, keeping the NEWEST content.
///
/// Two bounds compose: whole lines outside the line window are dropped, then
/// the head of the remaining body is cut until the byte budget fits with the
/// omission marker included. `lines_returned` counts lines with any content in
/// the final text (a line cut by the byte budget still counts, and its lost
/// head is reported in `omitted_bytes`).
fn page_logs(all: &[String], max_lines: usize, max_bytes: usize) -> LogPage {
    let total_in_ring = all.len();
    let start = total_in_ring.saturating_sub(max_lines.max(1));
    let window = &all[start..];
    let body_budget = max_bytes.saturating_sub(LOG_MARKER_RESERVE);

    // Take whole lines from the end while they fit the body budget.
    let mut kept_from = window.len();
    let mut body_len = 0usize;
    for i in (0..window.len()).rev() {
        let add = window[i].len() + usize::from(body_len > 0);
        if body_len + add > body_budget {
            break;
        }
        body_len += add;
        kept_from = i;
    }
    // Nothing whole fits: keep the tail of the newest line rather than
    // dropping it, because "the newest bytes" is what a tail reader wants and
    // a dropped line would report a zero-byte loss.
    if kept_from == window.len() && !window.is_empty() {
        kept_from = window.len() - 1;
    }
    let mut lines_returned = window.len() - kept_from;
    let mut body = window[kept_from..].join("\n");
    let mut omitted_bytes = 0usize;

    // One line can still exceed the whole body budget; cut its head (char
    // safe) and report the loss as bytes, not as a missing line.
    if body.len() > body_budget {
        let mut cut = body.len() - body_budget;
        while cut < body.len() && !body.is_char_boundary(cut) {
            cut += 1;
        }
        omitted_bytes += cut;
        body = body.get(cut..).unwrap_or_default().to_string();
    }

    // Build marker + body, then re-cut until the marker's own bytes fit too.
    let mut text = body;
    for _ in 0..4 {
        let omitted_lines = total_in_ring.saturating_sub(lines_returned);
        let truncated = omitted_lines > 0 || omitted_bytes > 0;
        let marker = if truncated {
            format!("… [omitted {omitted_lines} line(s), {omitted_bytes} byte(s)]\n")
        } else {
            String::new()
        };
        let combined = format!("{marker}{text}");
        if combined.len() <= max_bytes {
            return LogPage {
                bytes: combined.len(),
                text: combined,
                lines: lines_returned,
                truncated,
                omitted_lines,
                omitted_bytes,
            };
        }
        // Cut the overflow (plus the marker growth budget) from the body head.
        let overflow = combined.len() - max_bytes;
        let mut cut = overflow.min(text.len());
        while cut < text.len() && !text.is_char_boundary(cut) {
            cut += 1;
        }
        if cut == 0 {
            break;
        }
        omitted_bytes += cut;
        text = text.get(cut..).unwrap_or_default().to_string();
        // A cut may have emptied the oldest retained line: report it as a
        // missing line instead of a zero-length one.
        if text.ends_with('\n') {
            text.pop();
            lines_returned = lines_returned.saturating_sub(1);
        }
    }

    // Pathological budgets (marker longer than the reserve): keep the tail of
    // the marker+body splice and stay inside the budget.
    let omitted_lines = total_in_ring.saturating_sub(lines_returned);
    let truncated = omitted_lines > 0 || omitted_bytes > 0;
    let marker = if truncated {
        format!("… [omitted {omitted_lines} line(s), {omitted_bytes} byte(s)]\n")
    } else {
        String::new()
    };
    let mut combined = format!("{marker}{text}");
    if combined.len() > max_bytes {
        let mut cut = combined.len() - max_bytes;
        while cut < combined.len() && !combined.is_char_boundary(cut) {
            cut += 1;
        }
        combined = combined.get(cut..).unwrap_or_default().to_string();
    }
    LogPage {
        bytes: combined.len(),
        text: combined,
        lines: lines_returned,
        truncated,
        omitted_lines,
        omitted_bytes,
    }
}

/// Stable wire name for a drain kind.
fn shutdown_kind_name(kind: ShutdownKind) -> &'static str {
    match kind {
        ShutdownKind::Graceful => "graceful",
        ShutdownKind::Immediate => "immediate",
        ShutdownKind::Reload => "reload",
    }
}

/// What the drain arm accepts. `Reload` is deliberately absent: a reload is a
/// service-lifecycle signal read by the supervised program
/// (`DrainController::wait_for_reload`), it does NOT set the drain bit, so
/// treating it as a run action would report `accepted` while doing nothing.
fn parse_drain_kind(raw: Option<&str>) -> Result<ShutdownKind, &'static str> {
    match raw.map(str::trim).map(str::to_ascii_lowercase).as_deref() {
        None | Some("") | Some("graceful") => Ok(ShutdownKind::Graceful),
        Some("immediate") => Ok(ShutdownKind::Immediate),
        _ => Err("unsupported_kind"),
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
    /// Ids of runs that have left the registry (finished-and-evicted), kept so
    /// a caller cannot reuse a name that already addressed a different run.
    used_run_ids: Mutex<Vec<String>>,
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
        let Ok(mut used) = self.used_run_ids.lock() else {
            return Err("internal state poisoned");
        };
        if used.iter().any(|id| id == &handle.run_id) {
            return Err("run_id already used by an evicted run");
        }
        if runs.len() >= MAX_RUNS {
            if let Some(pos) = runs.iter().position(|r| r.is_finished()) {
                runs.remove(pos);
            } else {
                return Err("too many concurrent runs");
            }
        }
        used.push(handle.run_id.clone());
        if used.len() > MAX_REMEMBERED_RUN_IDS {
            used.remove(0);
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
    /// Signal a drain and wait (bounded) for the run to finish.
    ///
    /// `kind` is validated BEFORE anything is signalled: an unsupported kind
    /// must not report `accepted` while doing nothing (a `Reload` does not set
    /// the drain bit at all — see [`parse_drain_kind`]).
    async fn drain_payload(&self, args: DrainArgs) -> Value {
        let Some(run) = self.find_run(&args.run_id) else {
            return Self::error_payload("unknown_run", format!("no run with id `{}`", args.run_id));
        };
        let kind = match parse_drain_kind(args.kind.as_deref()) {
            Ok(kind) => kind,
            Err(code) => {
                return Self::error_payload(
                    code,
                    format!(
                        "drain kind {:?} is not supported — use `graceful` or `immediate`; a `reload` is a service-lifecycle signal for the supervised program, not a run action",
                        args.kind.as_deref().unwrap_or("")
                    ),
                );
            }
        };
        let already = run.drain.is_draining();
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
        // Finished or not, the snapshot comes from the same fleet sink the
        // `status` arm reads, so the worker objects keep an identical shape.
        let workers: Vec<Value> = run
            .fleet
            .snapshot()
            .iter()
            .map(|s| serde_json::to_value(s).unwrap_or(Value::Null))
            .collect();
        json!({
            "ok": true,
            "run_id": run.run_id,
            "accepted": true,
            "kind": shutdown_kind_name(kind),
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
        // Atomic accept: settle → pending check → budget check → record, all
        // in one critical section, so two concurrent calls cannot both win.
        if let Err(code) = run.accept_restart(&args.worker_id) {
            let phase = run
                .fleet
                .get(&args.worker_id)
                .map(|s| s.phase)
                .unwrap_or(WorkerPhase::Stopped);
            return Self::error_payload(
                code,
                format!(
                    "worker `{}` cannot restart now (phase {}, code {code})",
                    args.worker_id,
                    serde_json::to_value(phase)
                        .ok()
                        .and_then(|v| v.as_str().map(str::to_string))
                        .unwrap_or_else(|| "unknown".to_string())
                ),
            );
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
        // Page the whole ring: `page_logs` derives the window, the byte cut and
        // the reported counts from one consistent input.
        let all = ring.tail(ring.len());
        let total = all.len();
        let page = page_logs(&all, max_lines, max_bytes);
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
    /// Drain kind: `graceful` (default) or `immediate`. Any other value —
    /// `reload` included — is refused with `unsupported_kind` before anything
    /// is signalled; `reload` is a service-lifecycle signal for the supervised
    /// program, not an action on a supervision run.
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
        description = "Launch a set of child-process workers under malkuth's supervisor and run until they all exit (or the timeout elapses). Each worker is restarted per its policy (permanent/transient/temporary) with a sliding-window rate limit to prevent crash storms. Returns the final status snapshot of every worker plus a run_id. This call blocks for the lifetime of the supervision; while it is outstanding, malkuth_status / malkuth_logs / malkuth_restart / malkuth_drain act on the returned run_id (they are non-blocking and safe to issue concurrently). Registry limits: at most 8 runs are kept (a finished run is evicted first, taking its log rings with it), and a run_id cannot be reused once used."
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
            restart_book: Mutex::new(RestartBook::default()),
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
                    // there is no final snapshot to report. Publish that end
                    // into the fleet sink explicitly — the workers never got a
                    // chance to publish their own `stopped` phase, and a
                    // finished run must not keep claiming `running`.
                    handle.fleet.stop_all();
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
        description = "Non-blocking snapshot of every supervision run this MCP server knows about (or one run when `run_id` is passed). Per worker: id, kind, phase (starting/running/restarting/draining/stopped), pid, uptime_secs, restart_policy, restart_count, last_error. Every key is always present — pid and last_error are null when they do not apply, including for a finished run (whose workers report phase stopped). Safe to call while malkuth_supervise is still blocking. Answer: {\"ok\":true,\"runs\":[...]}, or {\"ok\":false,\"code\":\"unknown_run\",...}. Read-only: no side effects."
    )]
    async fn malkuth_status(
        &self,
        Parameters(args): Parameters<StatusArgs>,
        _context: RequestContext<RoleServer>,
    ) -> Result<CallToolResult, McpError> {
        Ok(Self::json_result(&self.status_payload(args).await))
    }

    #[tool(
        description = "Restart ONE worker of a running supervision, addressed by run_id + worker_id. Side effect: the current child receives a graceful stop (SIGTERM/CTRL_BREAK, escalating to a kill after 30s) and a fresh child is spawned under the same id; the supervisor's crash budget is NOT spent (it exists to damp crash loops) and the worker is never ended by this call. Operator budget: at most 5 accepted restarts per worker per 60s, checked synchronously (restart_budget_exhausted) — a refusal never queues silently. Coalescing: a request accepted while the worker is mid-restart/cooldown is applied as soon as the worker re-arms; it is never applied twice. Failure codes: unknown_run, unknown_worker, run_finished, worker_stopped, worker_draining, already_restarting, restart_budget_exhausted. Success: {\"ok\":true,\"accepted\":true,\"phase\":\"restarting\"}."
    )]
    async fn malkuth_restart(
        &self,
        Parameters(args): Parameters<RestartArgs>,
        _context: RequestContext<RoleServer>,
    ) -> Result<CallToolResult, McpError> {
        Ok(Self::json_result(&self.restart_payload(args).await))
    }

    #[tool(
        description = "Drain a running supervision and wait up to `timeout_secs` for workers to stop. `kind` is `graceful` (default: SIGTERM/CTRL_BREAK, up to 30s for in-flight work) or `immediate` (stop at once, kill after <=1s). Any other kind returns unsupported_kind before anything is signalled — notably `reload`, which is a service-lifecycle signal for the supervised program (it does not set the drain bit), not a run action. Idempotent: calling drain twice is safe and the second answer reports \"already\": true. Answer includes kind, draining, finished, timed_out and the per-worker snapshot."
    )]
    async fn malkuth_drain(
        &self,
        Parameters(args): Parameters<DrainArgs>,
        _context: RequestContext<RoleServer>,
    ) -> Result<CallToolResult, McpError> {
        Ok(Self::json_result(&self.drain_payload(args).await))
    }

    #[tool(
        description = "Read a BOUNDED tail of one worker's captured stdout/stderr (capture is on for MCP-supervised workers only). `lines` defaults to 100 (cap 500); `max_bytes` defaults to 8192 (cap 65536). The caps are exact: {\"bytes\": N} <= max_bytes including the marker, and lines_returned + omitted_lines == total_lines_in_ring. Anything dropped is reported in metadata (truncated, omitted_lines, omitted_bytes, total_lines_in_ring) and by a leading `… [omitted …]` marker inside text, so a caller can tell a window from the whole history and re-read with a larger budget if it matters. A line that the byte budget cuts is still counted in lines_returned, with its lost head in omitted_bytes. Failure codes: unknown_run, unknown_worker, worker_required (run has several workers and none was named)."
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
        used_run_ids: Mutex::new(Vec::new()),
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
            used_run_ids: Mutex::new(Vec::new()),
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
            restart_book: Mutex::new(RestartBook::default()),
            finished: Mutex::new(None),
        })
    }

    fn restart_args(run_id: &str, worker_id: &str) -> RestartArgs {
        RestartArgs {
            run_id: run_id.to_string(),
            worker_id: worker_id.to_string(),
        }
    }

    /// Keys every worker object must carry, in `status` *and* `drain` output.
    const WORKER_KEYS: [&str; 7] = [
        "id",
        "kind",
        "phase",
        "pid",
        "uptime_secs",
        "restart_count",
        "last_error",
    ];

    fn assert_worker_keys(worker: &Value) {
        for key in WORKER_KEYS {
            assert!(
                worker.get(key).is_some(),
                "worker snapshot is missing `{key}`: {worker}"
            );
        }
    }

    // ── status ───────────────────────────────────────────────

    #[tokio::test]
    async fn status_reports_every_worker_field() {
        let server = test_server();
        server
            .insert_run(run_with_workers(
                "r1",
                &[("beta", "backend"), ("alpha", "worker")],
            ))
            .unwrap();

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
        for worker in workers {
            assert_worker_keys(worker);
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

    /// S1-2 killer: a finished run must keep the full worker shape (`pid` and
    /// `uptime_secs` were dropped before; `drain` lost `restart_policy` too).
    #[tokio::test]
    async fn finished_run_keeps_every_snapshot_key() {
        let server = test_server();
        let handle = run_with_workers("rf", &[("api", "backend")]);
        handle.fleet.finish(
            "api",
            WorkerPhase::Stopped,
            2,
            Some("exit status: 9".into()),
        );
        handle.mark_finished(Vec::new());
        server.insert_run(handle).unwrap();

        let status = server.status_payload(StatusArgs { run_id: None }).await;
        let run = &status["runs"][0];
        assert_eq!(run["finished"], json!(true));
        assert!(run["finished_unix_secs"].as_u64().is_some());
        let worker = &run["workers"][0];
        assert_worker_keys(worker);
        assert_eq!(worker["phase"], json!("stopped"));
        assert_eq!(worker["pid"], Value::Null);
        assert_eq!(worker["uptime_secs"], json!(0));
        assert_eq!(worker["restart_policy"], json!("permanent"));
        assert_eq!(worker["restart_count"], json!(2));

        let drain = server
            .drain_payload(DrainArgs {
                run_id: "rf".into(),
                kind: None,
                timeout_secs: Some(0),
            })
            .await;
        assert_worker_keys(&drain["workers"][0]);
        assert_eq!(drain["already"], json!(false));
    }

    // ── drain ────────────────────────────────────────────────

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
        assert_eq!(first["kind"], json!("graceful"));

        // Idempotent: same run, second call, still accepted and truthful.
        let second = server
            .drain_payload(DrainArgs {
                run_id: "r2".into(),
                kind: Some("graceful".into()),
                timeout_secs: Some(0),
            })
            .await;
        assert_eq!(second["ok"], json!(true));
        assert_eq!(second["already"], json!(true));
        assert_eq!(second["draining"], json!(true));
    }

    /// S1-3 / S2-7 killer: `reload` used to be accepted while doing nothing
    /// (`begin_drain(Reload)` never sets the draining bit), and unknown kinds
    /// silently degraded to graceful. Both are refused before signalling;
    /// `immediate` is a real kind and must still work.
    #[tokio::test]
    async fn drain_kinds_are_whitelisted() {
        let server = test_server();
        server
            .insert_run(run_with_workers("rk", &[("svc", "backend")]))
            .unwrap();

        for bad in ["reload", "reload ", "graceful-ish", "SIGKILL"] {
            let payload = server
                .drain_payload(DrainArgs {
                    run_id: "rk".into(),
                    kind: Some(bad.into()),
                    timeout_secs: Some(0),
                })
                .await;
            assert_eq!(
                payload["code"],
                json!("unsupported_kind"),
                "kind `{bad}` must be refused: {payload}"
            );
            // A refused kind must not have signalled anything.
            let run = server.find_run("rk").unwrap();
            assert!(!run.drain.is_draining(), "kind `{bad}` signalled a drain");
            assert_eq!(run.drain.kind(), None);
        }

        let immediate = server
            .drain_payload(DrainArgs {
                run_id: "rk".into(),
                kind: Some("immediate".into()),
                timeout_secs: Some(0),
            })
            .await;
        assert_eq!(immediate["ok"], json!(true));
        assert_eq!(immediate["kind"], json!("immediate"));
        assert_eq!(immediate["draining"], json!(true));
        assert_eq!(
            server.find_run("rk").unwrap().drain.kind(),
            Some(ShutdownKind::Immediate)
        );
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

    // ── restart ──────────────────────────────────────────────

    #[tokio::test]
    async fn restart_accepts_running_worker_and_refuses_second_call() {
        let server = test_server();
        server
            .insert_run(run_with_workers("r3", &[("api", "backend")]))
            .unwrap();
        let first = server.restart_payload(restart_args("r3", "api")).await;
        assert_eq!(first["ok"], json!(true));
        assert_eq!(first["accepted"], json!(true));
        assert_eq!(first["phase"], json!("restarting"));

        let second = server.restart_payload(restart_args("r3", "api")).await;
        assert_eq!(second["ok"], json!(false));
        assert_eq!(second["code"], json!("already_restarting"));
    }

    #[tokio::test]
    async fn restart_failure_shapes_are_distinct() {
        let server = test_server();
        server
            .insert_run(run_with_workers("r4", &[("api", "backend")]))
            .unwrap();

        let unknown_run = server.restart_payload(restart_args("missing", "api")).await;
        assert_eq!(unknown_run["code"], json!("unknown_run"));

        let unknown_worker = server.restart_payload(restart_args("r4", "ghost")).await;
        assert_eq!(unknown_worker["code"], json!("unknown_worker"));

        // A finished run can no longer be steered.
        server.runs.lock().unwrap()[0].mark_finished(Vec::new());
        let finished = server.restart_payload(restart_args("r4", "api")).await;
        assert_eq!(finished["code"], json!("run_finished"));
    }

    /// S1-4 killer: the operator budget is checked synchronously and refuses
    /// with its own code (it is not the crash budget, which used to kill the
    /// worker after `accepted` had already been answered).
    #[tokio::test]
    async fn restart_budget_is_enforced() {
        let server = test_server();
        let handle = run_with_workers("rb", &[("w", "svc")]);
        server.insert_run(handle.clone()).unwrap();

        for i in 1..=EXPLICIT_RESTART_MAX {
            let payload = server.restart_payload(restart_args("rb", "w")).await;
            assert_eq!(payload["ok"], json!(true), "accept #{i}: {payload}");
            // Simulate the supervisor noticing: count advances, phase running.
            handle.fleet.restarting("w", i as u32);
            handle.fleet.running("w", Some(1000 + i as u32));
        }

        let refused = server.restart_payload(restart_args("rb", "w")).await;
        assert_eq!(refused["ok"], json!(false));
        assert_eq!(refused["code"], json!("restart_budget_exhausted"));
    }

    /// S2-9 / S2-2 killer: "never queued" must hold under REAL concurrency.
    ///
    /// The runtime must be multi-threaded and the tasks must be released
    /// together: on the default single-thread runtime the restart path has no
    /// `.await` before its critical section, so eight tasks run one after
    /// another and the test cannot see a lock-gap race (R2' control
    /// experiment: the old single-thread version stayed green against a
    /// check→unlock→sleep→relock→record mutation). With 8 worker threads plus
    /// a barrier, all eight enter the accept path simultaneously — exactly one
    /// may win.
    #[tokio::test(flavor = "multi_thread", worker_threads = 8)]
    async fn concurrent_restarts_accept_exactly_one() {
        const CALLERS: usize = 8;
        let server = Arc::new(test_server());
        server
            .insert_run(run_with_workers("rc", &[("w", "svc")]))
            .unwrap();
        let barrier = Arc::new(tokio::sync::Barrier::new(CALLERS));

        let mut tasks = Vec::new();
        for _ in 0..CALLERS {
            let server = server.clone();
            let barrier = barrier.clone();
            tasks.push(tokio::spawn(async move {
                // Release every caller at the same instant: the race window is
                // the critical section itself, not the task spawn order.
                barrier.wait().await;
                server.restart_payload(restart_args("rc", "w")).await
            }));
        }
        let mut accepted = 0;
        let mut refused = 0;
        for task in tasks {
            let payload = task.await.expect("join");
            if payload["ok"] == json!(true) {
                accepted += 1;
            } else {
                assert_eq!(payload["code"], json!("already_restarting"));
                refused += 1;
            }
        }
        assert_eq!(accepted, 1, "restart must be accepted at most once");
        assert_eq!(refused, CALLERS - 1);
    }

    #[tokio::test]
    async fn restart_refuses_a_worker_that_already_exited() {
        let server = test_server();
        let handle = run_with_workers("r5", &[("one", "svc")]);
        handle
            .fleet
            .finish("one", WorkerPhase::Stopped, 0, Some("exit 1".into()));
        server.insert_run(handle).unwrap();
        let payload = server.restart_payload(restart_args("r5", "one")).await;
        assert_eq!(payload["code"], json!("worker_stopped"));
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

    // ── logs ─────────────────────────────────────────────────

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
            text.contains("… [omitted 7 line(s)"),
            "missing truncation marker: {text}"
        );
    }

    /// S2-6 killer: after the byte budget cuts, the reported line/byte counts
    /// must be recomputed from the final text — and `bytes` must never exceed
    /// the requested budget (the marker used to push it over).
    #[tokio::test]
    async fn logs_byte_budget_is_exact_and_counted() {
        let server = test_server();
        let handle = run_with_workers("r7", &[("loud", "svc")]);
        let ring = handle.log_rings.get("loud").unwrap().clone();
        for i in 0..20 {
            ring.push(&format!("{i:02}-{}", "x".repeat(200)));
        }
        server.insert_run(handle).unwrap();

        for budget in [LOGS_MIN_BYTES, 512, 1024, LOGS_MAX_BYTES] {
            let payload = server
                .logs_payload(LogsArgs {
                    run_id: "r7".into(),
                    worker_id: Some("loud".into()),
                    lines: Some(LOGS_MAX_LINES as u32),
                    max_bytes: Some(budget as u32),
                })
                .await;
            assert_eq!(payload["ok"], json!(true));
            let bytes = payload["bytes"].as_u64().unwrap() as usize;
            let returned = payload["lines_returned"].as_u64().unwrap();
            let omitted_lines = payload["omitted_lines"].as_u64().unwrap();
            let total = payload["total_lines_in_ring"].as_u64().unwrap();
            assert!(
                bytes <= budget,
                "bytes {bytes} exceeded the {budget}-byte budget: {payload}"
            );
            assert_eq!(
                returned + omitted_lines,
                total,
                "line counts must balance: {payload}"
            );
            assert_eq!(
                payload["text"].as_str().unwrap().len(),
                bytes,
                "reported bytes must match the text"
            );
        }

        // A single line longer than the whole body budget is cut (not
        // dropped): it still counts as returned and its lost head is reported
        // as bytes.
        let long = run_with_workers("r7b", &[("long", "svc")]);
        long.log_rings.get("long").unwrap().push(&"z".repeat(5_000));
        server.insert_run(long).unwrap();
        let payload = server
            .logs_payload(LogsArgs {
                run_id: "r7b".into(),
                worker_id: Some("long".into()),
                lines: Some(10),
                max_bytes: Some(512),
            })
            .await;
        assert_eq!(payload["ok"], json!(true));
        assert_eq!(payload["lines_returned"], json!(1));
        assert_eq!(payload["omitted_lines"], json!(0));
        assert_eq!(payload["truncated"], json!(true));
        assert!(payload["bytes"].as_u64().unwrap() <= 512);
        assert!(
            payload["omitted_bytes"].as_u64().unwrap() > 0,
            "byte cut not reported: {payload}"
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
        let page = page_logs(&lines, lines.len(), LOGS_MIN_BYTES);
        assert!(page.truncated);
        assert!(std::str::from_utf8(page.text.as_bytes()).is_ok());
        assert!(page.text.ends_with("last"));
        assert!(page.bytes <= LOGS_MIN_BYTES);
    }

    /// Count invariants over a matrix of budgets (the S2-6 shape, unit level).
    #[test]
    fn page_logs_counts_always_balance() {
        let lines: Vec<String> = (0..40)
            .map(|i| format!("{i}-{}", "y".repeat(i * 7)))
            .collect();
        for max_lines in [1usize, 2, 5, 40, 500] {
            for max_bytes in [LOGS_MIN_BYTES, 400, 1024, 4096, LOGS_MAX_BYTES] {
                let page = page_logs(&lines, max_lines, max_bytes);
                assert!(
                    page.bytes <= max_bytes,
                    "lines={max_lines} bytes={max_bytes}: got {} bytes",
                    page.bytes
                );
                assert_eq!(page.text.len(), page.bytes);
                assert_eq!(page.lines + page.omitted_lines, lines.len());
                assert_eq!(
                    page.truncated,
                    page.omitted_lines > 0 || page.omitted_bytes > 0
                );
            }
        }
    }

    /// M1 killer: the caps are the context budget. Raising one silently
    /// widens what a model may be fed, so the documented numbers are pinned
    /// here — changing them is a deliberate edit that reddens this test.
    #[test]
    fn log_caps_are_contract_pinned() {
        assert_eq!(LOGS_DEFAULT_LINES, 100);
        assert_eq!(LOGS_MAX_LINES, 500);
        assert_eq!(LOGS_DEFAULT_BYTES, 8_192);
        assert_eq!(LOGS_MAX_BYTES, 65_536);
        assert_eq!(LOGS_MIN_BYTES, 256);
        // And the clamp the tools actually apply.
        let request = u32::MAX; // far above either cap
        let clamped_lines = (request as usize).clamp(1, LOGS_MAX_LINES);
        let clamped_bytes = (request as usize).clamp(LOGS_MIN_BYTES, LOGS_MAX_BYTES);
        assert_eq!(clamped_lines, 500);
        assert_eq!(clamped_bytes, 65_536);
    }

    // ── registry boundaries ──────────────────────────────────

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

    /// S2-10 killer: eviction must not recycle a name — an old id would then
    /// silently address a different run.
    #[tokio::test]
    async fn run_id_cannot_be_reused_after_eviction() {
        let server = test_server();
        for i in 0..MAX_RUNS {
            server
                .insert_run(run_with_workers(&format!("fill-{i}"), &[("w", "svc")]))
                .unwrap();
        }
        server.runs.lock().unwrap()[0].mark_finished(Vec::new());
        server
            .insert_run(run_with_workers("fresh", &[("w", "svc")]))
            .unwrap();
        // `fill-0` was evicted, but its id stays retired.
        let reuse = server.insert_run(run_with_workers("fill-0", &[("w", "svc")]));
        assert_eq!(reuse, Err("run_id already used by an evicted run"));
        assert!(server.find_run("fill-0").is_none());
    }
}
