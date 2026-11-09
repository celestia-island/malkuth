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

use crate::drain_auth::{ApprovalStatus, DrainAuthError, GateAction, GateDecision};
use crate::gate_grant::GrantRecord;
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
    /// failure code when the request cannot be accepted, or the supervisor
    /// `restart_count` the acceptance was armed against — the caller needs
    /// it to give the acceptance back if a later step fails.
    fn accept_restart(&self, worker_id: &str) -> Result<u32, &'static str> {
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
        Ok(count)
    }

    /// Roll back a restart acceptance whose later step (the approval spend)
    /// failed: nothing was signalled, so the pending entry and the budget
    /// stamp it committed must not outlive the failure — otherwise a healthy
    /// worker stays wedged on `already_restarting` until it exits on its
    /// own. Removes exactly the recorded `(worker_id, count_at_accept)`
    /// entry and, only if that entry was still live, that worker's newest
    /// stamp — a settled acceptance (already superseded by a later one)
    /// owns nothing to undo, and taking its stamp would burn the successor's
    /// budget.
    fn rollback_restart_accept(&self, worker_id: &str, count_at_accept: u32) {
        let Ok(mut book) = self.restart_book.lock() else {
            // A poisoned book cannot be unwedged from here either; nothing
            // further to undo.
            return;
        };
        if let Some(pos) = book
            .pending
            .iter()
            .rposition(|(id, count)| id == worker_id && *count == count_at_accept)
        {
            book.pending.remove(pos);

            if let Some(pos) = book.stamps.iter().rposition(|(id, _)| id == worker_id) {
                book.stamps.remove(pos);
            }
        }
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

/// What `malkuth_approve` accepts for the decision field. An omitted or empty
/// decision is REFUSED: absence must never read as consent.
fn parse_gate_decision(raw: Option<&str>) -> Result<GateDecision, &'static str> {
    match raw.map(str::trim).map(str::to_ascii_lowercase).as_deref() {
        None | Some("") => Err("approval_decision_required"),
        Some("allow") | Some("approved") => Ok(GateDecision::Allow),
        Some("review") => Ok(GateDecision::Review),
        Some("block") | Some("blocked") => Ok(GateDecision::Block),
        Some("unblock") => Ok(GateDecision::Unblock),
        _ => Err("unsupported_decision"),
    }
}

/// What `malkuth_approve` accepts for the action field: the approval names the
/// one action it authorizes, and can only be spent on that action.
fn parse_gate_action(raw: Option<&str>) -> Result<GateAction, &'static str> {
    match raw.map(str::trim).map(str::to_ascii_lowercase).as_deref() {
        None | Some("") => Err("approval_action_required"),
        Some("restart") => Ok(GateAction::Restart),
        Some("drain") => Ok(GateAction::Drain),
        _ => Err("unsupported_action"),
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
    /// The ed25519 public key (lowercase 64-hex) grant signatures are checked
    /// against — resolved once at construction from
    /// `MALKUTH_GATE_VERIFY_KEY` (falling back to the built-in production
    /// key), so a malformed override fails the server, never silently
    /// weakens verification.
    gate_verify_key: String,
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
        // What this call spent: `(proposal_id, worker_id)` when a real spend
        // happened (an idempotent retry of an already-draining run never
        // spends, so it must never write a `done` receipt either).
        let mut spent = None;
        // An already-draining run is a no-op retry: answer idempotently and do
        // not spend the approval (nothing was done).
        if !already {
            let proposal_id = match Self::require_proposal(args.proposal_id.as_deref()) {
                Ok(id) => id,
                Err(payload) => return payload,
            };
            let worker_id = match args.worker_id.as_deref().map(str::trim) {
                Some(id) if !id.is_empty() => id.to_string(),
                _ => {
                    return Self::error_payload(
                        DrainAuthError::InvalidRequest.code(),
                        "a drain must name the worker whose approved proposal justifies it",
                    );
                }
            };
            // Validate AND spend in one step: a refusal never consumes, and a
            // second, real drain with the same approval answers `approval_consumed`.
            // The live identity of the judged worker (from THIS run's fleet —
            // the drain acts on this run) rides along: an attested approval
            // only spends on the exact (pid, starttime) its grant names.
            let live_identity = run
                .fleet
                .get(&worker_id)
                .and_then(|s| s.pid.map(u64::from).zip(s.starttime));
            if let Err(error) = crate::drain_auth::validate_drain_request(
                &worker_id,
                &proposal_id,
                None,
                live_identity,
                crate::drain_auth::global(),
            ) {
                return Self::error_payload(error.code(), error.to_string());
            }
            spent = Some((proposal_id, worker_id));
        }
        run.drain.begin_drain(kind);
        // The synchronous acceptance is out: pair the spend's pending receipt
        // with its `done`.
        if let Some((proposal_id, worker_id)) = &spent {
            crate::drain_auth::global().spend_completed(proposal_id, worker_id, GateAction::Drain);
        }
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

    /// Register one approval; nothing is executed here.
    ///
    /// P0b: a spendable decision (`allow`/`review`) must carry a valid grant —
    /// an ed25519 signature from the gate issuer over the canonical grant
    /// message binding proposal + worker + action + the live worker identity
    /// `(pid, starttime)`. The live identity is resolved from the supervised
    /// runs at approve time: exactly one live worker with that id must exist,
    /// which also pins the approve-after-supervise ordering. `block` records a
    /// refusal and needs no grant.
    fn approve_payload(&self, args: &ApproveArgs) -> Value {
        let proposal_id = args.proposal_id.trim();
        let worker_id = args.worker_id.trim();
        if proposal_id.is_empty() || worker_id.is_empty() {
            return Self::error_payload(
                DrainAuthError::InvalidRequest.code(),
                "proposal_id and worker_id must both be non-empty",
            );
        }
        let action = match parse_gate_action(args.action.as_deref()) {
            Ok(action) => action,
            Err(code) => {
                return Self::error_payload(
                    code,
                    "action is required and must be restart or drain — the approval names the one action it authorizes",
                );
            }
        };
        let decision = match parse_gate_decision(args.decision.as_deref()) {
            Ok(decision) => decision,
            Err(code) => {
                let message = if code == "approval_decision_required" {
                    "decision is required and must be allow, review, block, or unblock — an omitted decision is refused, not defaulted"
                } else {
                    "decision must be allow, review, block, or unblock"
                };
                return Self::error_payload(code, message);
            }
        };
        let unattested =
            |message: String| Self::error_payload(DrainAuthError::Unattested.code(), message);

        // `block` refuses everything and `unblock` lifts exactly that; neither
        // authorizes an action, so neither needs an attestation. Everything
        // spendable goes through the grant path.
        let grant = if matches!(decision, GateDecision::Block | GateDecision::Unblock) {
            None
        } else {
            let Some(sig) = args.sig.as_deref().map(str::trim).filter(|s| !s.is_empty()) else {
                return unattested(
                    "an allow/review approval must carry the issuer's grant signature (`sig` + `issued_at`)".into(),
                );
            };
            let Some(issued_at) = args.issued_at else {
                return unattested(
                    "an allow/review approval must carry the grant's `issued_at` (unix seconds)"
                        .into(),
                );
            };
            // The canonical grant message embeds the proposal id as bare
            // 32-hex (scepter's `gate_proposal_id` shape); anything else can
            // never have been signed. Lowercase-strict like every other hex
            // in the grant chain — the issuer only ever mints lowercase.
            if proposal_id.len() != 32
                || proposal_id
                    .bytes()
                    .any(|b| !matches!(b, b'0'..=b'9' | b'a'..=b'f'))
            {
                return unattested(
                    "proposal_id must be the bare 32-hex gate_proposal_id for a signed grant"
                        .into(),
                );
            }
            // Resolve the live worker identity across every known run. Every
            // live incarnation carrying this worker id must agree on exactly
            // one (pid, starttime): none means the worker is not supervised
            // (or between children), disagreeing duplicates mean the id is
            // ambiguous — both refuse.
            let Ok(runs_locked) = self.runs.lock() else {
                return Self::error_payload(
                    "internal_state_poisoned",
                    "runs lock poisoned while binding the grant",
                );
            };
            let mut identities: Vec<(u64, u64)> = runs_locked
                .iter()
                .filter_map(|r| r.fleet.get(worker_id))
                .filter_map(|s| s.pid.map(u64::from).zip(s.starttime))
                .collect();
            identities.sort_unstable();
            identities.dedup();
            let [identity] = identities[..] else {
                return unattested(format!(
                    "no single live supervised worker with id `{worker_id}` to bind the grant to ({})",
                    if identities.is_empty() {
                        "none found"
                    } else {
                        "conflicting incarnations"
                    }
                ));
            };
            let message = crate::gate_grant::grant_message(
                proposal_id,
                worker_id,
                action.as_str(),
                identity.0,
                identity.1,
                issued_at,
            );
            if let Err(error) =
                crate::gate_grant::verify_with_key(&message, sig, &self.gate_verify_key)
            {
                return unattested(format!(
                    "the grant signature does not verify against this gate's key ({error})"
                ));
            }
            Some(GrantRecord {
                pid: identity.0,
                starttime: identity.1,
                issued_at,
                digest: crate::gate_grant::grant_digest(&message),
                sig: sig.to_string(),
            })
        };
        let registry = crate::drain_auth::global();
        if let Err(error) = registry.approve(proposal_id, worker_id, action, decision, grant) {
            return Self::error_payload(error.code(), error.to_string());
        }
        json!({
            "ok": true,
            "registered": true,
            "proposal_id": proposal_id,
            "worker_id": worker_id,
            "action": action.as_str(),
            "decision": match decision {
                GateDecision::Allow => "allow",
                GateDecision::Review => "review",
                GateDecision::Block => "block",
                GateDecision::Unblock => "unblock",
            },
            "expires_in_secs": registry.ttl_secs(),
        })
    }

    /// Refuse an action that does not name a proposal, with the stable gate
    /// code. The refusal states the requirement; it does not teach the caller
    /// how to obtain one (that guidance let an unattended client self-serve
    /// the gate).
    fn require_proposal(proposal_id: Option<&str>) -> Result<String, Value> {
        match proposal_id.map(str::trim) {
            Some(id) if !id.is_empty() => Ok(id.to_string()),
            _ => Err(Self::error_payload(
                DrainAuthError::Required.code(),
                "this action requires an approved proposal_id",
            )),
        }
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
        let proposal_id = match Self::require_proposal(args.proposal_id.as_deref()) {
            Ok(id) => id,
            Err(payload) => return payload,
        };
        // The gate is checked (not spent) before the atomic accept: a refusal
        // below must not burn an approval, and a spent approval must not
        // authorize an action twice. The status carries the action: an
        // approval issued for a drain never vouches for a restart.
        let registry = crate::drain_auth::global();
        match registry.status(&proposal_id, &args.worker_id, GateAction::Restart) {
            ApprovalStatus::Approved => {}
            other => {
                let code = match other {
                    ApprovalStatus::Expired => "approval_expired",
                    ApprovalStatus::Consumed => "approval_consumed",
                    ApprovalStatus::Blocked => "approval_blocked",
                    ApprovalStatus::WrongAction => "approval_action_mismatch",
                    _ => "approval_required",
                };

                return Self::error_payload(code, format!("the approval is not live ({code})"));
            }
        }
        // P0b identity pre-check, BEFORE the accept: `accept_restart` commits
        // restart budget and arms the signal, so an attested approval must
        // already match the live incarnation here. A mismatch refuses without
        // touching the worker or the approval (the accept→consume gap below
        // re-checks approval liveness atomically — the identity it compares
        // against is this same snapshot, so a respawn inside that
        // preemption-only, `.await`-free window — microseconds — is accepted;
        // if the spend loses, the acceptance is rolled back below — no wedge,
        // no budget spent).
        let live_identity = run
            .fleet
            .get(&args.worker_id)
            .and_then(|s| s.pid.map(u64::from).zip(s.starttime));
        if let Some(expected) =
            registry.attested_identity(&proposal_id, &args.worker_id, GateAction::Restart)
        {
            if live_identity != Some(expected) {
                return Self::error_payload(
                    DrainAuthError::Unattested.code(),
                    "the live worker identity does not match the grant's attested (pid, starttime)",
                );
            }
        }
        // Atomic accept: settle → pending check → budget check → record, all
        // in one critical section, so two concurrent calls cannot both win.
        let count_at_accept = match run.accept_restart(&args.worker_id) {
            Ok(count_at_accept) => count_at_accept,
            Err(code) => {
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
        };
        // The action is accepted: now spend the approval. Anything that failed
        // above left it whole. A failure HERE (a concurrent block, drain, or
        // second spend of the same approval — the registry re-checks liveness
        // atomically under its write lock) must also give the acceptance
        // back: nothing was signalled yet, so a leftover pending entry would
        // wedge the worker on `already_restarting` until it exits on its own.
        if let Err(error) = registry.consume(
            &proposal_id,
            &args.worker_id,
            GateAction::Restart,
            live_identity,
        ) {
            run.rollback_restart_accept(&args.worker_id, count_at_accept);
            return Self::error_payload(error.code(), error.to_string());
        }
        signal.notify_one();
        // The acceptance is out: pair the spend's pending receipt with its
        // `done` (the pending/done gap is the crash window P1's reconciliation
        // reads as 已花未做).
        registry.spend_completed(&proposal_id, &args.worker_id, GateAction::Restart);
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
struct ApproveArgs {
    /// The approval token (scepter's `gate_proposal_id`): a bare 32-hex string.
    proposal_id: String,
    /// The one worker this approval authorizes.
    worker_id: String,
    /// Required — the one action this approval authorizes: `restart` or
    /// `drain`. The approval can only be spent on that action.
    #[serde(default)]
    action: Option<String>,
    /// Required — `allow`, `review`, or `block`. An omitted decision is
    /// refused (`approval_decision_required`), never defaulted.
    #[serde(default)]
    decision: Option<String>,
    /// The issuer's grant signature (ed25519, 128 hex chars) over the
    /// canonical grant message. Required for `allow`/`review` — a spendable
    /// approval without a verifying grant is refused
    /// (`approval_unattested`). Not needed for `block`.
    #[serde(default)]
    sig: Option<String>,
    /// The grant's issue time (unix seconds), signed into the grant message.
    /// Required with `sig`; a grant outside the clock window (60 s future
    /// skew, TTL past) is refused (`approval_epoch_stale`).
    #[serde(default)]
    issued_at: Option<u64>,
}

#[derive(Debug, Deserialize, JsonSchema)]
struct RestartArgs {
    /// Run id returned by malkuth_supervise.
    run_id: String,
    /// Worker id to restart (must exist in that run).
    worker_id: String,
    /// The approval this action spends (registered with `malkuth_approve`).
    /// Required: an action without a live, target-bound approval is refused.
    #[serde(default)]
    proposal_id: Option<String>,
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
    /// The approval this drain spends (registered with `malkuth_approve`).
    /// Required: an action without a live, target-bound approval is refused.
    #[serde(default)]
    proposal_id: Option<String>,
    /// Which worker's approval justifies draining the run. Required: an
    /// approval is bound to one target, so a run-wide drain must name the
    /// judged worker it is acting on.
    #[serde(default)]
    worker_id: Option<String>,
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
    /// Byte budget for the returned text: default 8192, capped at 65536, with a
    /// floor of 256 — a request below 256 is raised to 256, so the returned page
    /// can be larger than the number asked for. Older bytes are cut at a char
    /// boundary and counted in `omitted_bytes`.
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
        description = "Register ONE target-bound approval for a future restart/drain. `proposal_id` is the approval token (scepter's `gate_proposal_id`: a bare 32-hex string), `worker_id` the single worker it authorizes, `action` (required) restart or drain — the approval is spent by that action only, and `decision` (required) allow / review / block; omitting either field is refused. `allow`/`review` must additionally carry the gate issuer's grant: `sig` (ed25519 over the canonical grant message binding proposal + worker + action + the worker's live pid/starttime) and `issued_at`; a missing/non-verifying signature answers approval_unattested, a grant outside the clock window answers approval_epoch_stale, and exactly one live supervised worker with that id must exist. `block` records a refusal and needs no grant. An approval is live for 300 s, is bound to exactly that proposal + worker + action (+ pid/starttime when granted), and authorizes AT MOST ONE action: the gate spends it on success, and a replay answers approval_consumed. A grant can be registered until its clock window closes, and the registered approval then lives its own 300 s TTL, so worst case a grant stays spendable until ~issuance + 600 s. This tool only records the approval — it performs nothing."
    )]
    async fn malkuth_approve(
        &self,
        Parameters(args): Parameters<ApproveArgs>,
        _context: RequestContext<RoleServer>,
    ) -> Result<CallToolResult, McpError> {
        Ok(Self::json_result(&self.approve_payload(&args)))
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
        description = "Read a BOUNDED tail of one worker's captured stdout/stderr (capture is on for MCP-supervised workers only). `lines` defaults to 100 (cap 500); `max_bytes` defaults to 8192 (cap 65536) with a floor of 256 — a request below 256 is raised to 256, so `bytes` is bounded by max(max_bytes, 256). The caps are exact: {\"bytes\": N} <= max(max_bytes, 256) including the marker, and lines_returned + omitted_lines == total_lines_in_ring. Anything dropped is reported in metadata (truncated, omitted_lines, omitted_bytes, total_lines_in_ring) and by a leading `… [omitted …]` marker inside text, so a caller can tell a window from the whole history and re-read with a larger budget if it matters. A line that the byte budget cuts is still counted in lines_returned, with its lost head in omitted_bytes. Failure codes: unknown_run, unknown_worker, worker_required (run has several workers and none was named)."
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
        gate_verify_key: crate::gate_grant::resolve_verify_key()?,
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
            gate_verify_key: crate::gate_grant::tests::TEST_VERIFY_KEY_HEX.to_string(),
        }
    }

    fn run_with_workers(run_id: &str, workers: &[(&str, &str)]) -> Arc<RunHandle> {
        run_with_workers_at(run_id, workers, std::process::id())
    }

    /// Like [`run_with_workers`], but the workers run under a chosen pid —
    /// grant tests stage a second run of the same worker id under a
    /// DIFFERENT live process (pid 1 is always present in the container and
    /// is never this test process).
    fn run_with_workers_at(run_id: &str, workers: &[(&str, &str)], pid: u32) -> Arc<RunHandle> {
        let fleet = FleetState::new();
        let mut restart_signals = HashMap::new();
        let mut log_rings = HashMap::new();
        // The default fixture pid is this test process itself: `/proc/<pid>/stat`
        // then always resolves, so the worker's `starttime` is a REAL value
        // the grant tests can sign over (host-dependent but deterministic
        // within a test run — no fixed constant could be).
        for (id, kind) in workers {
            let spec = WorkerSpec::new(*id, *kind, "/bin/true");
            fleet.register(&spec);
            fleet.running(id, Some(pid));
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

    /// A token no test ever registers, so refusal paths cannot be polluted by a
    /// concurrent test that did register it.
    fn unique_token() -> String {
        static SEQ: AtomicU64 = AtomicU64::new(0);
        let seq = SEQ.fetch_add(1, Ordering::Relaxed) + 1;
        format!("unregistered-{}-{seq}", std::process::id())
    }

    /// Approve a throwaway proposal for this worker and hand back its id: the
    /// gate now requires one, and a fresh id per call keeps tests independent
    /// (a spent approval may never authorize a second action).
    fn approval_for(worker_id: &str, action: GateAction) -> String {
        static SEQ: AtomicU64 = AtomicU64::new(0);
        let seq = SEQ.fetch_add(1, Ordering::Relaxed) + 1;
        let proposal = format!("gate-test-{}-{seq}", std::process::id());
        crate::drain_auth::global()
            .approve(&proposal, worker_id, action, GateDecision::Allow, None)
            .unwrap();
        proposal
    }

    fn restart_args(run_id: &str, worker_id: &str) -> RestartArgs {
        RestartArgs {
            run_id: run_id.to_string(),
            worker_id: worker_id.to_string(),
            proposal_id: Some(approval_for(worker_id, GateAction::Restart)),
        }
    }

    /// A restart request with no approval at all (the refusal path).
    fn unapproved_restart_args(run_id: &str, worker_id: &str) -> RestartArgs {
        RestartArgs {
            run_id: run_id.to_string(),
            worker_id: worker_id.to_string(),
            proposal_id: None,
        }
    }

    fn drain_args(run_id: &str, worker_id: &str, kind: Option<&str>) -> DrainArgs {
        DrainArgs {
            run_id: run_id.to_string(),
            kind: kind.map(str::to_string),
            timeout_secs: Some(0),
            proposal_id: Some(approval_for(worker_id, GateAction::Drain)),
            worker_id: Some(worker_id.to_string()),
        }
    }

    /// Keys every worker object must carry, in `status` *and* `drain` output.
    const WORKER_KEYS: [&str; 8] = [
        "id",
        "kind",
        "phase",
        "pid",
        "uptime_secs",
        "restart_count",
        "last_error",
        "starttime",
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
        assert_eq!(workers[0]["pid"], json!(std::process::id()));
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

        let drain = server.drain_payload(drain_args("rf", "svc", None)).await;
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
        let first = server.drain_payload(drain_args("r2", "only", None)).await;
        assert_eq!(first["ok"], json!(true));
        assert_eq!(first["accepted"], json!(true));
        assert_eq!(first["already"], json!(false));
        assert_eq!(first["draining"], json!(true));
        assert_eq!(first["kind"], json!("graceful"));

        // Idempotent: same run, second call, still accepted and truthful.
        let second = server
            .drain_payload(drain_args("r2", "only", Some("graceful")))
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
                .drain_payload(drain_args("rk", "svc", Some(bad)))
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
            .drain_payload(drain_args("rk", "svc", Some("immediate")))
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
            .drain_payload(drain_args("nope", "missing", None))
            .await;
        assert_eq!(payload["code"], json!("unknown_run"));
    }

    // ── restart ──────────────────────────────────────────────

    // ── approval gate ───────────────────────────────────────

    #[tokio::test]
    async fn an_action_without_an_approval_is_refused() {
        let server = test_server();
        server
            .insert_run(run_with_workers("g1", &[("api", "backend")]))
            .unwrap();

        let restart = server
            .restart_payload(unapproved_restart_args("g1", "api"))
            .await;
        assert_eq!(restart["ok"], json!(false));
        assert_eq!(restart["code"], json!("approval_required"));
        // The refusal states the requirement; it must not teach an unattended
        // caller how to satisfy it.
        let message = restart["message"].as_str().unwrap_or_default();
        assert!(
            !message.contains("malkuth_approve"),
            "refusal message must not point at the approve tool: {message}"
        );

        let drain = server
            .drain_payload(DrainArgs {
                run_id: "g1".into(),
                kind: None,
                timeout_secs: Some(0),
                proposal_id: None,
                worker_id: None,
            })
            .await;
        assert_eq!(drain["ok"], json!(false));
        assert_eq!(drain["code"], json!("approval_required"));
        // Nothing was signalled by either refusal.
        assert!(!server.find_run("g1").unwrap().drain.is_draining());
    }

    #[tokio::test]
    async fn a_restart_spends_its_approval_exactly_once() {
        let server = test_server();
        server
            .insert_run(run_with_workers("g2", &[("api", "backend")]))
            .unwrap();
        let proposal = approval_for("api", GateAction::Restart);
        let args = || RestartArgs {
            run_id: "g2".into(),
            worker_id: "api".into(),
            proposal_id: Some(proposal.clone()),
        };

        let first = server.restart_payload(args()).await;
        assert_eq!(first["ok"], json!(true));
        assert_eq!(first["accepted"], json!(true));

        // The same approval may not authorize a second action, even once the
        // worker is ready again (that is what one-approval-one-action means).
        let replay = server.restart_payload(args()).await;
        assert_eq!(replay["ok"], json!(false));
        assert_eq!(replay["code"], json!("approval_consumed"));
    }

    #[tokio::test]
    async fn a_refused_restart_leaves_the_approval_unspent() {
        let server = test_server();
        let run = run_with_workers("g3", &[("api", "backend")]);
        // A draining worker cannot be restarted; the refusal must not burn the
        // approval, so the same proposal still works once the worker is running.
        run.fleet.draining("api");
        server.insert_run(run.clone()).unwrap();

        let proposal = approval_for("api", GateAction::Restart);
        let args = || RestartArgs {
            run_id: "g3".into(),
            worker_id: "api".into(),
            proposal_id: Some(proposal.clone()),
        };
        let refused = server.restart_payload(args()).await;
        assert_eq!(refused["code"], json!("worker_draining"));
        assert_eq!(
            crate::drain_auth::global().status(&proposal, "api", GateAction::Restart),
            ApprovalStatus::Approved,
            "a refused action must not spend the approval"
        );

        run.fleet.running("api", None);
        let accepted = server.restart_payload(args()).await;
        assert_eq!(accepted["ok"], json!(true));
    }

    #[tokio::test]
    async fn a_drain_spends_its_approval_and_repeats_idempotently() {
        let server = test_server();
        server
            .insert_run(run_with_workers("g4", &[("svc", "backend")]))
            .unwrap();
        let proposal = approval_for("svc", GateAction::Drain);
        let args = || DrainArgs {
            run_id: "g4".into(),
            kind: None,
            timeout_secs: Some(0),
            proposal_id: Some(proposal.clone()),
            worker_id: Some("svc".into()),
        };

        let first = server.drain_payload(args()).await;
        assert_eq!(first["ok"], json!(true));
        assert_eq!(first["already"], json!(false));

        // A retry is a no-op: still accepted, still truthful, and it does not
        // try to spend the (already spent) approval again.
        let retry = server.drain_payload(args()).await;
        assert_eq!(retry["ok"], json!(true));
        assert_eq!(retry["already"], json!(true));
    }

    #[tokio::test]
    async fn the_gate_rejects_a_forged_or_expired_approval() {
        let server = test_server();
        server
            .insert_run(run_with_workers("g5", &[("api", "backend")]))
            .unwrap();

        let forged = server
            .restart_payload(RestartArgs {
                run_id: "g5".into(),
                worker_id: "api".into(),
                proposal_id: Some(unique_token()),
            })
            .await;
        assert_eq!(forged["code"], json!("approval_required"));

        let bound_elsewhere = approval_for("other-worker", GateAction::Restart);
        let wrong_worker = server
            .restart_payload(RestartArgs {
                run_id: "g5".into(),
                worker_id: "api".into(),
                proposal_id: Some(bound_elsewhere.clone()),
            })
            .await;
        assert_eq!(wrong_worker["code"], json!("approval_required"));
        assert_eq!(
            crate::drain_auth::global().status(
                &bound_elsewhere,
                "other-worker",
                GateAction::Restart
            ),
            ApprovalStatus::Approved,
            "a mismatched worker must not spend the real target's approval"
        );
    }

    #[tokio::test]
    async fn an_approval_cannot_authorize_the_other_action() {
        let server = test_server();
        server
            .insert_run(run_with_workers("g6", &[("api", "backend")]))
            .unwrap();

        // Registered for a drain, spent on a restart attempt: refused with its
        // own code — and the refusal must not spend the real approval.
        let proposal = approval_for("api", GateAction::Drain);
        let refused = server
            .restart_payload(RestartArgs {
                run_id: "g6".into(),
                worker_id: "api".into(),
                proposal_id: Some(proposal.clone()),
            })
            .await;
        assert_eq!(refused["ok"], json!(false));
        assert_eq!(refused["code"], json!("approval_action_mismatch"));
        assert_eq!(
            crate::drain_auth::global().status(&proposal, "api", GateAction::Drain),
            ApprovalStatus::Approved,
            "a mismatched action must not spend the real approval"
        );
    }

    #[test]
    fn approving_takes_both_ids_and_a_known_decision() {
        let server = test_server();

        // A block registers without any grant: it authorizes nothing.
        let blocked_token = unique_token();
        let blocked = server.approve_payload(&ApproveArgs {
            proposal_id: blocked_token.clone(),
            worker_id: "api".into(),
            action: Some("drain".into()),
            decision: Some("block".into()),
            sig: None,
            issued_at: None,
        });
        assert_eq!(blocked["ok"], json!(true));
        assert_eq!(blocked["registered"], json!(true));
        assert_eq!(blocked["action"], json!("drain"));
        assert_eq!(blocked["decision"], json!("block"));
        assert_eq!(blocked["expires_in_secs"], json!(300));
        assert_eq!(
            crate::drain_auth::global().status(&blocked_token, "api", GateAction::Drain),
            ApprovalStatus::Blocked,
            "a block is a real, stored decision — not a missing entry"
        );

        // P1: the block survives contact with the decision plumbing. A fresh
        // allow on the blocked pair cannot lift it — at the registry level that
        // refusal is `approval_blocked` (drain_auth tests), and at this
        // boundary an unattested allow is refused even earlier; only the
        // explicit `unblock` decision lifts the pair.
        let unblocked = server.approve_payload(&ApproveArgs {
            proposal_id: blocked_token.clone(),
            worker_id: "api".into(),
            action: Some("drain".into()),
            decision: Some("unblock".into()),
            sig: None,
            issued_at: None,
        });
        assert_eq!(unblocked["ok"], json!(true));
        assert_eq!(unblocked["decision"], json!("unblock"));
        assert_eq!(
            crate::drain_auth::global().status(&blocked_token, "api", GateAction::Drain),
            ApprovalStatus::Unknown,
            "an explicit unblock lifts the pair cleanly"
        );

        // An omitted decision must never read as consent.
        let no_decision = server.approve_payload(&ApproveArgs {
            proposal_id: unique_token(),
            worker_id: "api".into(),
            action: Some("drain".into()),
            decision: None,
            sig: None,
            issued_at: None,
        });
        assert_eq!(no_decision["code"], json!("approval_decision_required"));

        // An omitted action is refused too: the approval names its action.
        let no_action = server.approve_payload(&ApproveArgs {
            proposal_id: unique_token(),
            worker_id: "api".into(),
            action: None,
            decision: Some("allow".into()),
            sig: None,
            issued_at: None,
        });
        assert_eq!(no_action["code"], json!("approval_action_required"));

        let empty = server.approve_payload(&ApproveArgs {
            proposal_id: "  ".into(),
            worker_id: "api".into(),
            action: Some("restart".into()),
            decision: Some("allow".into()),
            sig: None,
            issued_at: None,
        });
        assert_eq!(empty["code"], json!("invalid_request"));

        let bad = server.approve_payload(&ApproveArgs {
            proposal_id: unique_token(),
            worker_id: "api".into(),
            action: Some("restart".into()),
            decision: Some("maybe".into()),
            sig: None,
            issued_at: None,
        });
        assert_eq!(bad["code"], json!("unsupported_decision"));

        let bad_action = server.approve_payload(&ApproveArgs {
            proposal_id: unique_token(),
            worker_id: "api".into(),
            action: Some("hibernate".into()),
            decision: Some("allow".into()),
            sig: None,
            issued_at: None,
        });
        assert_eq!(bad_action["code"], json!("unsupported_action"));

        // P0b: a spendable decision without a grant signature is refused —
        // allow/review may never register unattested again.
        for decision in ["allow", "review"] {
            let unattested = server.approve_payload(&ApproveArgs {
                proposal_id: "0123456789abcdef0123456789abcdef".into(),
                worker_id: "api".into(),
                action: Some("restart".into()),
                decision: Some(decision.into()),
                sig: None,
                issued_at: None,
            });
            assert_eq!(unattested["ok"], json!(false));
            assert_eq!(unattested["code"], json!("approval_unattested"));
        }
    }

    // ── P0b: the grant boundary ─────────────────────────────

    /// A fresh 32-hex proposal id per test (the grant message only carries
    /// bare 32-hex ids, and the registry is global across parallel tests).
    fn hex_proposal() -> String {
        static SEQ: AtomicU64 = AtomicU64::new(0);
        let seq = SEQ.fetch_add(1, Ordering::Relaxed) + 1;
        format!("{seq:032x}")
    }

    fn unix_now() -> u64 {
        SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .unwrap()
            .as_secs()
    }

    /// The live identity a run's snapshot actually reports for a worker —
    /// host-dependent (real `/proc` starttime), so grant tests sign over what
    /// this returns, never a constant.
    fn live_identity(run: &RunHandle, worker_id: &str) -> (u64, u64) {
        let snapshot = run.fleet.get(worker_id).expect("worker registered");
        let pid = u64::from(snapshot.pid.expect("worker carries a pid"));
        let starttime = snapshot.starttime.expect("worker carries a starttime");
        (pid, starttime)
    }

    /// `allow` args carrying a runtime-signed grant over the given identity.
    fn signed_grant_args(
        worker_id: &str,
        action: GateAction,
        proposal: &str,
        identity: (u64, u64),
        issued_at: u64,
    ) -> ApproveArgs {
        let message = crate::gate_grant::grant_message(
            proposal,
            worker_id,
            action.as_str(),
            identity.0,
            identity.1,
            issued_at,
        );
        ApproveArgs {
            proposal_id: proposal.into(),
            worker_id: worker_id.into(),
            action: Some(action.as_str().into()),
            decision: Some("allow".into()),
            sig: Some(crate::gate_grant::tests::sign_test_message(&message)),
            issued_at: Some(issued_at),
        }
    }

    #[tokio::test]
    async fn a_signed_grant_registers_and_spends_on_restart() {
        let server = test_server();
        let run = run_with_workers("p1", &[("api", "backend")]);
        let identity = live_identity(&run, "api");
        server.insert_run(run).unwrap();

        let proposal = hex_proposal();
        let payload = server.approve_payload(&signed_grant_args(
            "api",
            GateAction::Restart,
            &proposal,
            identity,
            unix_now(),
        ));
        assert_eq!(payload["ok"], json!(true), "{payload}");
        assert_eq!(payload["decision"], json!("allow"));

        // The granted approval actually authorizes its action.
        let restart = server
            .restart_payload(RestartArgs {
                run_id: "p1".into(),
                worker_id: "api".into(),
                proposal_id: Some(proposal.clone()),
            })
            .await;
        assert_eq!(restart["ok"], json!(true), "{restart}");

        // …exactly once.
        assert_eq!(
            crate::drain_auth::global().status(&proposal, "api", GateAction::Restart),
            ApprovalStatus::Consumed
        );
    }

    #[tokio::test]
    async fn a_signed_drain_grant_spends_on_drain() {
        let server = test_server();
        let run = run_with_workers("p2", &[("api", "backend")]);
        let identity = live_identity(&run, "api");
        server.insert_run(run).unwrap();

        let proposal = hex_proposal();
        let payload = server.approve_payload(&signed_grant_args(
            "api",
            GateAction::Drain,
            &proposal,
            identity,
            unix_now(),
        ));
        assert_eq!(payload["ok"], json!(true), "{payload}");

        let mut args = drain_args("p2", "api", None);
        args.proposal_id = Some(proposal.clone());
        let drain = server.drain_payload(args).await;
        assert_eq!(drain["ok"], json!(true), "{drain}");
        assert_eq!(
            crate::drain_auth::global().status(&proposal, "api", GateAction::Drain),
            ApprovalStatus::Consumed
        );
    }

    /// A grant minted for ANOTHER identity (another process) verifies against
    /// nothing this gate serves — negative ⑤ of the plan.
    #[tokio::test]
    async fn a_grant_signed_over_another_identity_is_unattested() {
        let server = test_server();
        let run = run_with_workers("p3", &[("api", "backend")]);
        let _real_identity = live_identity(&run, "api");
        server.insert_run(run).unwrap();

        // Sign over an identity that is NOT the live one (respawned twin).
        let forged = signed_grant_args(
            "api",
            GateAction::Restart,
            &hex_proposal(),
            (4_242_424, 1_234_567),
            unix_now(),
        );
        let payload = server.approve_payload(&forged);
        assert_eq!(payload["ok"], json!(false));
        assert_eq!(payload["code"], json!("approval_unattested"), "{payload}");
    }

    /// No run, no identity to bind — the issuer may not pre-sign for a worker
    /// that is not supervised (approve-after-supervise ordering).
    #[test]
    fn an_unknown_worker_cannot_be_granted() {
        let server = test_server();
        let args = signed_grant_args(
            "ghost",
            GateAction::Restart,
            &hex_proposal(),
            (1, 1),
            unix_now(),
        );
        let payload = server.approve_payload(&args);
        assert_eq!(payload["ok"], json!(false));
        assert_eq!(payload["code"], json!("approval_unattested"), "{payload}");
    }

    /// A worker between children (no live pid/starttime) cannot be granted —
    /// its next incarnation would be a different process.
    #[tokio::test]
    async fn a_worker_between_children_cannot_be_granted() {
        let server = test_server();
        let run = run_with_workers("p4", &[("api", "backend")]);
        run.fleet.restarting("api", 1);
        server.insert_run(run).unwrap();

        let args = signed_grant_args(
            "api",
            GateAction::Restart,
            &hex_proposal(),
            (1, 1),
            unix_now(),
        );
        let payload = server.approve_payload(&args);
        assert_eq!(payload["ok"], json!(false));
        assert_eq!(payload["code"], json!("approval_unattested"), "{payload}");
    }

    /// A perfectly signed grant outside the clock window is refused — in both
    /// directions.
    #[tokio::test]
    async fn stale_grants_are_refused_by_epoch() {
        let server = test_server();
        let run = run_with_workers("p5", &[("api", "backend")]);
        let identity = live_identity(&run, "api");
        server.insert_run(run).unwrap();

        for issued_at in [unix_now() + 400, unix_now().saturating_sub(1_000)] {
            let payload = server.approve_payload(&signed_grant_args(
                "api",
                GateAction::Restart,
                &hex_proposal(),
                identity,
                issued_at,
            ));
            assert_eq!(payload["ok"], json!(false), "{payload}");
            assert_eq!(payload["code"], json!("approval_epoch_stale"));
        }
    }

    /// The restart pre-check fires BEFORE the accept: an identity change after
    /// approve refuses without signalling a restart or spending anything.
    #[tokio::test]
    async fn a_restart_refuses_when_identity_changed_before_spend() {
        let server = test_server();
        let run = run_with_workers("p6", &[("api", "backend")]);
        let identity = live_identity(&run, "api");
        server.insert_run(run.clone()).unwrap();

        let proposal = hex_proposal();
        let payload = server.approve_payload(&signed_grant_args(
            "api",
            GateAction::Restart,
            &proposal,
            identity,
            unix_now(),
        ));
        assert_eq!(payload["ok"], json!(true), "{payload}");

        // The worker ends (its pid leaves the snapshot) before the restart.
        run.fleet.finish("api", WorkerPhase::Stopped, 0, None);
        let refused = server
            .restart_payload(RestartArgs {
                run_id: "p6".into(),
                worker_id: "api".into(),
                proposal_id: Some(proposal.clone()),
            })
            .await;
        assert_eq!(refused["ok"], json!(false));
        assert_eq!(refused["code"], json!("approval_unattested"), "{refused}");
        assert_eq!(
            crate::drain_auth::global().status(&proposal, "api", GateAction::Restart),
            ApprovalStatus::Approved,
            "the refusal must not spend the approval"
        );
    }

    /// Same law on the drain side: consume-time mismatch refuses and spends
    /// nothing.
    #[tokio::test]
    async fn a_drain_refuses_when_identity_changed_before_spend() {
        let server = test_server();
        let run = run_with_workers("p7", &[("api", "backend")]);
        let identity = live_identity(&run, "api");
        server.insert_run(run.clone()).unwrap();

        let proposal = hex_proposal();
        let payload = server.approve_payload(&signed_grant_args(
            "api",
            GateAction::Drain,
            &proposal,
            identity,
            unix_now(),
        ));
        assert_eq!(payload["ok"], json!(true), "{payload}");

        run.fleet.finish("api", WorkerPhase::Stopped, 0, None);
        let mut args = drain_args("p7", "api", None);
        args.proposal_id = Some(proposal.clone());
        let refused = server.drain_payload(args).await;
        assert_eq!(refused["ok"], json!(false));
        assert_eq!(refused["code"], json!("approval_unattested"), "{refused}");
        assert_eq!(
            crate::drain_auth::global().status(&proposal, "api", GateAction::Drain),
            ApprovalStatus::Approved,
            "the refusal must not spend the approval"
        );
    }

    /// Two runs of the SAME worker id under two different live processes: the
    /// identity scan must see two distinct (pid, starttime) pairs and refuse
    /// — a grant for either one would be a coin flip over which incarnation
    /// gets spent on.
    #[tokio::test]
    async fn conflicting_live_incarnations_cannot_be_granted() {
        let server = test_server();
        let mine = run_with_workers("p8", &[("api", "backend")]);
        let twin = run_with_workers_at("p9", &[("api", "backend")], 1);
        let identity = live_identity(&mine, "api");
        let twin_identity = live_identity(&twin, "api");
        assert_ne!(
            identity, twin_identity,
            "fixture precondition: pid 1 must resolve to a distinct live identity"
        );
        server.insert_run(mine).unwrap();
        server.insert_run(twin).unwrap();

        // Perfectly signed for OUR incarnation — the conflict alone refuses.
        let payload = server.approve_payload(&signed_grant_args(
            "api",
            GateAction::Restart,
            &hex_proposal(),
            identity,
            unix_now(),
        ));
        assert_eq!(payload["ok"], json!(false));
        assert_eq!(payload["code"], json!("approval_unattested"), "{payload}");
        assert!(
            payload["message"]
                .as_str()
                .unwrap_or_default()
                .contains("conflicting"),
            "the refusal must name the conflict, not claim the worker is unsupervised: {payload}"
        );
    }

    /// The restart pre-check must fire while the worker is STILL Running: a
    /// respawn under a new pid would leave `accept_restart` succeeding (and
    /// burning budget on a grant minted for the previous incarnation) if the
    /// check only caught Stopped workers.
    #[tokio::test]
    async fn a_restart_refuses_when_identity_changed_while_still_running() {
        let server = test_server();
        let run = run_with_workers("pa", &[("api", "backend")]);
        let identity = live_identity(&run, "api");
        server.insert_run(run.clone()).unwrap();

        let proposal = hex_proposal();
        let payload = server.approve_payload(&signed_grant_args(
            "api",
            GateAction::Restart,
            &proposal,
            identity,
            unix_now(),
        ));
        assert_eq!(payload["ok"], json!(true), "{payload}");

        // The worker respawns under pid 1 (still Running, new identity).
        run.fleet.running("api", Some(1));
        let refused = server
            .restart_payload(RestartArgs {
                run_id: "pa".into(),
                worker_id: "api".into(),
                proposal_id: Some(proposal.clone()),
            })
            .await;
        assert_eq!(refused["ok"], json!(false));
        assert_eq!(refused["code"], json!("approval_unattested"), "{refused}");
        assert_eq!(
            crate::drain_auth::global().status(&proposal, "api", GateAction::Restart),
            ApprovalStatus::Approved,
            "the refusal must not spend the approval"
        );

        // And nothing was accepted: a fresh grant for the NEW incarnation
        // restarts immediately — a phantom pending restart would answer
        // `already_restarting` here instead.
        let new_identity = live_identity(&run, "api");
        assert_ne!(
            identity, new_identity,
            "fixture precondition: respawn moved the identity"
        );
        let second_proposal = hex_proposal();
        let second = server.approve_payload(&signed_grant_args(
            "api",
            GateAction::Restart,
            &second_proposal,
            new_identity,
            unix_now(),
        ));
        assert_eq!(second["ok"], json!(true), "{second}");
        let restarted = server
            .restart_payload(RestartArgs {
                run_id: "pa".into(),
                worker_id: "api".into(),
                proposal_id: Some(second_proposal),
            })
            .await;
        assert_eq!(restarted["ok"], json!(true), "{restarted}");
        assert_eq!(restarted["accepted"], json!(true));
    }

    /// Two runs of the SAME worker id under the SAME live process (run
    /// replacement / snapshot overlap): the incarnations AGREE on one
    /// (pid, starttime), so the dedup in the identity scan must fold them
    /// into one grantable identity — a conflict refusal here would break
    /// issuance exactly during approve-after-supervise transitions.
    #[tokio::test]
    async fn agreeing_duplicate_incarnations_register() {
        let server = test_server();
        // BOTH incarnations live under pid 1, so they AGREE on one
        // (pid, starttime): the dedup in the identity scan must fold them
        // into one grantable identity — a conflict refusal here would break
        // issuance exactly during approve-after-supervise transitions.
        let mine = run_with_workers_at("pd", &[("api", "backend")], 1);
        let echo = run_with_workers_at("pe", &[("api", "backend")], 1);
        let identity = live_identity(&echo, "api");
        assert_eq!(
            live_identity(&mine, "api"),
            identity,
            "fixture: both incarnations must report the SAME identity"
        );
        server.insert_run(mine).unwrap();
        server.insert_run(echo).unwrap();

        // Signed over the pid-1 identity BOTH runs of the second incarnation
        // report — this is exactly the duplicate the dedup exists for.
        let payload = server.approve_payload(&signed_grant_args(
            "api",
            GateAction::Restart,
            &hex_proposal(),
            identity,
            unix_now(),
        ));
        assert_eq!(payload["ok"], json!(true), "{payload}");

        // And the grant spends normally.
        let proposal = payload["proposal_id"].as_str().unwrap().to_string();
        let restart = server
            .restart_payload(RestartArgs {
                run_id: "pd".into(),
                worker_id: "api".into(),
                proposal_id: Some(proposal.clone()),
            })
            .await;
        assert_eq!(restart["ok"], json!(true), "{restart}");
        assert_eq!(
            crate::drain_auth::global().status(&proposal, "api", GateAction::Restart),
            ApprovalStatus::Consumed
        );
    }

    /// Proposal shapes that can never have been signed: wrong length, non-hex
    /// characters, and UPPERCASE hex (the whole grant chain is
    /// lowercase-strict — the issuer only ever mints lowercase).
    #[tokio::test]
    async fn malformed_proposal_shapes_cannot_be_granted() {
        let server = test_server();
        let run = run_with_workers("pb", &[("api", "backend")]);
        let identity = live_identity(&run, "api");
        server.insert_run(run).unwrap();

        let bad_proposals = [
            "0".repeat(31),                 // one short
            format!("{}g", "0".repeat(31)), // non-hex char
            "0F".repeat(16),                // uppercase hex
            "0".repeat(33),                 // one long
        ];
        for bad in &bad_proposals {
            // Signed over the malformed id anyway — the shape check must
            // refuse before any signature work.
            let payload = server.approve_payload(&signed_grant_args(
                "api",
                GateAction::Restart,
                bad,
                identity,
                unix_now(),
            ));
            assert_eq!(payload["ok"], json!(false), "proposal {bad:?}: {payload}");
            assert_eq!(payload["code"], json!("approval_unattested"));
            assert!(
                payload["message"]
                    .as_str()
                    .unwrap_or_default()
                    .contains("32-hex"),
                "the refusal must point at the proposal shape: {payload}"
            );
        }

        // Doubly invalid — malformed proposal AND a garbage signature — must
        // still blame the proposal shape: the shape gate runs first, so
        // reordering verification ahead of it (which would surface the
        // signature error instead) is a pinned observable difference.
        for bad in &bad_proposals {
            let mut args = signed_grant_args("api", GateAction::Restart, bad, identity, unix_now());
            args.sig = Some("00".repeat(64));
            let payload = server.approve_payload(&args);
            assert_eq!(payload["ok"], json!(false), "proposal {bad:?}: {payload}");
            assert!(
                payload["message"]
                    .as_str()
                    .unwrap_or_default()
                    .contains("32-hex"),
                "shape refusal must precede verification: {payload}"
            );
        }
    }

    /// `review` spends exactly like `allow`: the grant requirement covers
    /// both spendable decisions, so a valid grant must register a review.
    #[tokio::test]
    async fn a_review_decision_spends_like_allow_with_a_grant() {
        let server = test_server();
        let run = run_with_workers("pc", &[("api", "backend")]);
        let identity = live_identity(&run, "api");
        server.insert_run(run).unwrap();

        let proposal = hex_proposal();
        let mut args =
            signed_grant_args("api", GateAction::Restart, &proposal, identity, unix_now());
        args.decision = Some("review".into());
        let payload = server.approve_payload(&args);
        assert_eq!(payload["ok"], json!(true), "{payload}");
        assert_eq!(payload["decision"], json!("review"));

        let restart = server
            .restart_payload(RestartArgs {
                run_id: "pc".into(),
                worker_id: "api".into(),
                proposal_id: Some(proposal.clone()),
            })
            .await;
        assert_eq!(restart["ok"], json!(true), "{restart}");
        assert_eq!(
            crate::drain_auth::global().status(&proposal, "api", GateAction::Restart),
            ApprovalStatus::Consumed
        );
    }

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

    /// Round-3 S3 killer: `rollback_restart_accept` must return exactly the
    /// acceptance it was handed. A live acceptance gives back BOTH its
    /// pending entry and its stamp; a settled acceptance (superseded by a
    /// later one whose accept removed the old pending entry via retain)
    /// owns nothing — rolling it back must leave the successor's pending
    /// entry AND its budget stamp untouched.
    #[test]
    fn rollback_returns_a_live_acceptance_and_never_a_settled_one() {
        let server = test_server();
        let handle = run_with_workers("rbk", &[("w", "svc")]);
        server.insert_run(handle.clone()).unwrap();

        let live_pendings = |worker: &str| {
            handle
                .restart_book
                .lock()
                .unwrap()
                .pending
                .iter()
                .filter(|(id, _)| id == worker)
                .count()
        };
        let stamps_of = |worker: &str| {
            handle
                .restart_book
                .lock()
                .unwrap()
                .stamps
                .iter()
                .filter(|(id, _)| id == worker)
                .count()
        };

        // Live acceptance: both the pending entry and the stamp come back.
        let count = handle.accept_restart("w").expect("accept");
        assert_eq!(live_pendings("w"), 1);
        assert_eq!(stamps_of("w"), 1);
        handle.rollback_restart_accept("w", count);
        assert_eq!(live_pendings("w"), 0, "pending entry returned");
        assert_eq!(stamps_of("w"), 0, "budget stamp returned");

        // Settled acceptance: the supervisor finished that restart and a
        // successor acceptance is live. Rolling back the OLD acceptance
        // owns nothing — the restart really happened, so its budget stamp
        // stays spent, and above all the successor's pending entry AND its
        // stamp must not be touched.
        let first = handle.accept_restart("w").expect("re-accept");
        handle.fleet.restarting("w", first + 1);
        handle.fleet.running("w", Some(4242));
        let second = handle.accept_restart("w").expect("successor accept");
        assert_eq!(second, first + 1, "supervisor counter moved");
        assert_eq!(live_pendings("w"), 1, "old entry settled by retain");
        assert_eq!(stamps_of("w"), 2, "both restarts spent budget");

        handle.rollback_restart_accept("w", first);
        assert_eq!(
            live_pendings("w"),
            1,
            "settled rollback keeps the successor pending"
        );
        assert_eq!(
            stamps_of("w"),
            2,
            "settled rollback keeps every stamp — its restart really happened"
        );

        // The successor's own rollback still works normally.
        handle.rollback_restart_accept("w", second);
        assert_eq!(live_pendings("w"), 0);
        assert_eq!(stamps_of("w"), 1, "only the settled stamp stays spent");
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
        // The floor is part of the contract too: a request below it is raised
        // to it, so a page may be larger than the number asked for.
        assert_eq!(16usize.clamp(LOGS_MIN_BYTES, LOGS_MAX_BYTES), 256);
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
