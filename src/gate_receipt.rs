//! The gate receipt journal: durable, append-only evidence of every gate
//! spend, block, unblock, and spend-time refusal.
//!
//! The registry's in-memory state answers "is this approval live *right
//! now*"; the journal answers "what did the gate do, in what order, across
//! restarts". They are deliberately separate: the journal only ever grows
//! (P2's reconciliation reads it back), and a write that cannot be persisted
//! refuses the spend it was supposed to evidence — `ReceiptSink::append`
//! returning `Err` is the sink's veto (`Err ⇒ 拒绝兑现`), the registry then
//! un-spends the approval so the caller can retry once the sink recovers.
//!
//! Persistence is opt-in by deployment: `MALKUTH_GATE_JOURNAL` names a JSONL
//! file. Without it the process falls back to an in-memory sink — same
//! semantics while the process lives, nothing survives a restart (this is the
//! pre-P1 status quo, kept for tests and non-supervisor embedders). Blocks
//! replayed from the journal at construction are what `rehydrate_blocks`
//! means: a block outlives the process that issued it, and no TTL or capacity
//! pressure ever removes one — only an explicit `unblock` does.
//!
//! The epoch is minted by malkuth itself at registry construction (pid +
//! monotonic-ish clock), never from caller-supplied fields: receipts from an
//! earlier life of the process are tellable from this one at a glance.

use serde::{Deserialize, Serialize};
use std::io::Write as _;
use std::path::{Path, PathBuf};
use std::sync::Mutex;

/// What kind of gate event a receipt evidences.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ReceiptKind {
    /// The approval was spent and the action was accepted to start — written
    /// atomically with the spend, BEFORE the action runs. A `pending` receipt
    /// with no `done` after it means "spent, completion unknown" (已花未做).
    Pending,
    /// The action's synchronous acceptance completed (signal sent / drain
    /// started). Best-effort: its absence after a `pending` is the crash
    /// window P2 reconciles.
    Done,
    /// An operator blocked this proposal + worker pair. Blocks replay at
    /// startup and are immortal until `unblock`.
    Blocked,
    /// An explicit `unblock` lifted a block.
    Unblocked,
    /// A spend was refused at the gate (e.g. the pair is blocked) — the
    /// refusal is archived, not silently dropped (拒绝入档).
    Refused,
}

impl ReceiptKind {
    /// The stable wire name (also the journal spelling).
    #[must_use]
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Pending => "pending",
            Self::Done => "done",
            Self::Blocked => "blocked",
            Self::Unblocked => "unblocked",
            Self::Refused => "refused",
        }
    }
}

/// One append-only gate event. `seq` is assigned by the sink (monotonic per
/// journal), `epoch` by malkuth at registry construction — a receipt is
/// located by `(epoch, seq)` and neither field is derived from caller input.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Receipt {
    pub seq: u64,
    pub epoch: String,
    pub kind: ReceiptKind,
    pub proposal_id: String,
    pub worker_id: String,
    /// The action the receipt is about (`restart` / `drain`); `None` for
    /// pair-wide events (a block covers both actions).
    pub action: Option<String>,
    /// The stable gate code, for `refused` receipts (e.g.
    /// `approval_blocked`); `None` elsewhere.
    pub code: Option<String>,
    /// Unix seconds at write time, from malkuth's own clock.
    pub at: u64,
}

/// Acknowledgement for one persisted receipt: the sink-assigned sequence
/// number inside the sink's namespace.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct ReceiptAck {
    pub seq: u64,
}

/// Why a receipt could not be persisted. Carries no secret material — the
/// journal never holds seeds or signatures, only gate metadata.
#[derive(Debug, thiserror::Error)]
pub enum ReceiptError {
    /// The underlying store refused the write (io error text embedded).
    #[error("the receipt journal refused the write: {0}")]
    Io(String),
    /// The sink's internal lock is poisoned; the journal may be mid-line.
    #[error("the receipt journal's lock is poisoned")]
    Poisoned,
}

/// Where receipts go. Every append is synchronous: the caller's control flow
/// (spend or refuse) waits for the durability answer. The spend path holds the
/// registry's entries write lock across the append, so a slow sink stalls the
/// whole gate — blocking is the fail-closed direction (an unanswered append
/// never becomes a spent approval), but sinks are expected to be local and
/// fast, not network-backed.
pub trait ReceiptSink: Send + Sync {
    /// Persist one receipt. `Err` vetoes what the caller was about to do (for
    /// spends) or is surfaced as degradation (for blocks, which are inserted
    /// in-memory regardless — protection must not be lost to a journal
    /// outage).
    ///
    /// # Errors
    ///
    /// [`ReceiptError`] when the store cannot take the line.
    fn append(&self, receipt: &Receipt) -> Result<ReceiptAck, ReceiptError>;
}

/// The in-process sink: same semantics as the file journal while the process
/// lives, nothing survives a restart. This is the default when
/// `MALKUTH_GATE_JOURNAL` is unset, and the test double everywhere else.
#[derive(Default)]
pub struct MemoryReceiptSink {
    state: Mutex<MemoryState>,
}

#[derive(Default)]
struct MemoryState {
    receipts: Vec<Receipt>,
    next_seq: u64,
}

impl MemoryReceiptSink {
    #[must_use]
    pub fn new() -> Self {
        Self::default()
    }

    /// The receipts so far, in append order (test/inspection access).
    #[must_use]
    pub fn receipts(&self) -> Vec<Receipt> {
        self.state
            .lock()
            .map(|s| s.receipts.clone())
            .unwrap_or_default()
    }
}

impl ReceiptSink for MemoryReceiptSink {
    fn append(&self, receipt: &Receipt) -> Result<ReceiptAck, ReceiptError> {
        let mut state = self.state.lock().map_err(|_| ReceiptError::Poisoned)?;
        state.next_seq += 1;
        let mut stored = receipt.clone();
        stored.seq = state.next_seq;
        state.receipts.push(stored);
        Ok(ReceiptAck {
            seq: state.next_seq,
        })
    }
}

/// A sink that always fails: the test double for "the journal is down".
#[derive(Default)]
pub struct FailingReceiptSink {
    state: Mutex<MemoryState>,
}

impl FailingReceiptSink {
    #[must_use]
    pub fn new() -> Self {
        Self::default()
    }

    /// What callers *tried* to append — every attempt, none persisted.
    #[must_use]
    pub fn attempts(&self) -> Vec<Receipt> {
        self.state
            .lock()
            .map(|s| s.receipts.clone())
            .unwrap_or_default()
    }
}

impl ReceiptSink for FailingReceiptSink {
    fn append(&self, receipt: &Receipt) -> Result<ReceiptAck, ReceiptError> {
        if let Ok(mut state) = self.state.lock() {
            state.receipts.push(receipt.clone());
        }
        Err(ReceiptError::Io(
            "journal unavailable (forced test failure)".into(),
        ))
    }
}

/// JSONL journal: one receipt per line, append + flush + `sync_data` per
/// append. Sequence numbers continue from the last line already on disk, so
/// restarting against the same file keeps `(epoch, seq)` unique.
pub struct FileReceiptJournal {
    path: PathBuf,
    inner: Mutex<FileState>,
}

struct FileState {
    file: std::fs::File,
    next_seq: u64,
    corrupt_lines: u64,
}

impl FileReceiptJournal {
    /// Open (or create) the journal at `path`, continuing its sequence from
    /// the highest `seq` already stored.
    ///
    /// # Errors
    ///
    /// [`ReceiptError::Io`] when the file cannot be opened — the caller
    /// decides whether that is fatal. The startup seam
    /// (`drain_auth::registry_from_journal_path`) logs loudly and falls back
    /// to the in-memory sink: the gate keeps working, but receipts will not
    /// survive a restart until the journal path is fixed.
    pub fn open(path: impl Into<PathBuf>) -> Result<Self, ReceiptError> {
        let path = path.into();
        let file = std::fs::OpenOptions::new()
            .create(true)
            .append(true)
            .open(&path)
            .map_err(|e| ReceiptError::Io(format!("{}: {e}", path.display())))?;
        let (mut next_seq, mut corrupt_lines) = (0u64, 0u64);
        let bytes = std::fs::read(&path).map_err(|e| ReceiptError::Io(e.to_string()))?;
        for line in String::from_utf8_lossy(&bytes).lines() {
            if line.trim().is_empty() {
                continue;
            }
            match serde_json::from_str::<Receipt>(line) {
                Ok(r) => next_seq = next_seq.max(r.seq),
                Err(_) => corrupt_lines += 1,
            }
        }
        Ok(Self {
            path,
            inner: Mutex::new(FileState {
                file,
                next_seq,
                corrupt_lines,
            }),
        })
    }

    /// The absolute path the journal writes to.
    #[must_use]
    pub fn path(&self) -> &Path {
        &self.path
    }

    /// Corrupt (unparseable) lines skipped at open — surfaced, not silent:
    /// a hand-truncated tail must not quietly reset the sequence.
    #[must_use]
    pub fn corrupt_lines(&self) -> u64 {
        self.inner.lock().map(|s| s.corrupt_lines).unwrap_or(0)
    }

    /// Read the journal back: the PARSEABLE receipts, in file order. Lines that
    /// fail to parse are skipped silently here and counted in
    /// [`Self::corrupt_lines`] — an audit that must see corruption counts those
    /// lines separately; [`reconcile`] over this output sees only the well-formed
    /// tail of the file. Used by startup rehydration and by P2's reconciliation.
    ///
    /// # Errors
    ///
    /// [`ReceiptError::Io`] when the file cannot be read.
    pub fn read_all(&self) -> Result<Vec<Receipt>, ReceiptError> {
        let bytes = std::fs::read(&self.path)
            .map_err(|e| ReceiptError::Io(format!("{}: {e}", self.path.display())))?;
        let mut receipts = Vec::new();
        for line in String::from_utf8_lossy(&bytes).lines() {
            if line.trim().is_empty() {
                continue;
            }
            if let Ok(r) = serde_json::from_str::<Receipt>(line) {
                receipts.push(r);
            }
        }
        Ok(receipts)
    }
}

impl ReceiptSink for FileReceiptJournal {
    fn append(&self, receipt: &Receipt) -> Result<ReceiptAck, ReceiptError> {
        let mut state = self.inner.lock().map_err(|_| ReceiptError::Poisoned)?;
        // The number is claimed before the write: a failed append leaves a
        // permanent (cosmetic) gap in the journal — (epoch, seq) uniqueness is
        // preserved and replay takes the max, so nothing reads the gap.
        state.next_seq += 1;
        let mut stored = receipt.clone();
        stored.seq = state.next_seq;
        let line = serde_json::to_string(&stored)
            .map_err(|e| ReceiptError::Io(format!("serialize: {e}")))?;
        writeln!(state.file, "{line}").map_err(|e| ReceiptError::Io(e.to_string()))?;
        state
            .file
            .flush()
            .and_then(|_| state.file.sync_data())
            .map_err(|e| ReceiptError::Io(e.to_string()))?;
        Ok(ReceiptAck {
            seq: state.next_seq,
        })
    }
}

/// The blocks a journal replay carries: `(proposal_id, worker_id)` pairs that
/// must be live again before the registry answers anything.
pub type ReplayedBlocks = Vec<(String, String)>;

/// P2 reconciliation: what the journal admits to when audited end to end.
///
/// Every finding is a fact about the FILE, not an accusation — several of them
/// (sequence gaps, pending-without-done) are the documented crash/failed-append
/// windows, and the point of the audit is that they are LISTED for a human, not
/// silently absorbed by replay's max-seq shortcut.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Reconciliation {
    /// Sequence discontinuities, in file order: each is (epoch, the seq the
    /// previous line carried, the seq this line carries) with `next != prev + 1`
    /// (a first line above 1 is a gap after 0). A failed append claims its
    /// number before writing, so a gap is a permanent, cosmetic-but-real mark —
    /// the audit surfaces it, replay ignores it (it takes the max).
    pub seq_gaps: Vec<(String, u64, u64)>,
    /// Approval batches with no completion receipt: the latest `pending` for an
    /// (epoch, proposal, worker, action) whose kind never reaches `done` after
    /// it — the 已花未做 crash window (spent, completion unknown).
    pub pending_without_done: Vec<(String, String, String, String)>,
    /// Completion receipts with no approval batch: a `done` whose
    /// (epoch, proposal, worker, action) has no `pending` before it — the file
    /// records an acceptance the journal never approved. Under append-only
    /// writing this is tampering or foreign input, never a crash artifact.
    pub done_without_pending: Vec<(String, String, String, String)>,
}

/// Reconcile a journal read ([`FileReceiptJournal::read_all`], file order):
/// sequence gaps, approval batches whose completion receipt is missing, and
/// completion receipts without an approval batch. Pure over the input — the
/// caller decides what a finding means for their surface (a human audit page,
/// a startup warning, an RPC answer).
#[must_use]
pub fn reconcile(receipts: &[Receipt]) -> Reconciliation {
    let mut reconciliation = Reconciliation {
        seq_gaps: Vec::new(),
        pending_without_done: Vec::new(),
        done_without_pending: Vec::new(),
    };
    // Sequence continuity, per epoch, in file order (the journal is append-only,
    // so file order IS sequence order within an epoch).
    let mut last_seq: Option<(&str, u64)> = None;
    // Batch ledger: the latest pending per (epoch, proposal, worker, action).
    let mut open_batches: std::collections::HashSet<(String, String, String, String)> =
        std::collections::HashSet::new();
    for receipt in receipts {
        let (last_epoch, last) = match last_seq {
            Some((epoch, seq)) => (epoch, seq),
            None => ("", 0),
        };
        if last_epoch != receipt.epoch {
            // An epoch switch restarts the numbering; only a first line above 1
            // is a gap within its own epoch — checked when the epoch is seen.
            if receipt.seq > 1 {
                reconciliation
                    .seq_gaps
                    .push((receipt.epoch.clone(), 0, receipt.seq));
            }
        } else if receipt.seq != last + 1 {
            reconciliation
                .seq_gaps
                .push((receipt.epoch.clone(), last, receipt.seq));
        }
        last_seq = Some((receipt.epoch.as_str(), receipt.seq));

        let action = receipt.action.clone().unwrap_or_default();
        let key = (
            receipt.epoch.clone(),
            receipt.proposal_id.clone(),
            receipt.worker_id.clone(),
            action,
        );
        match receipt.kind {
            ReceiptKind::Pending => {
                open_batches.insert(key);
            }
            ReceiptKind::Done => {
                if open_batches.remove(&key) {
                    // Batch closed: nothing owed.
                } else {
                    reconciliation.done_without_pending.push(key);
                }
            }
            ReceiptKind::Blocked | ReceiptKind::Unblocked | ReceiptKind::Refused => {
                // Pair-wide / refusal events carry no approval batch.
            }
        }
    }
    reconciliation.pending_without_done = open_batches.into_iter().collect();
    reconciliation
        .pending_without_done
        .sort_by(|a, b| (a.0.as_str(), &a.1, &a.2, &a.3).cmp(&(b.0.as_str(), &b.1, &b.2, &b.3)));
    reconciliation
}

/// Replay a journal for startup: the block pairs it contains, and the highest
/// sequence seen (so a fresh sink continues the numbering).
///
/// # Errors
///
/// [`ReceiptError::Io`] when the file cannot be read.
pub fn replay_blocks(path: &Path) -> Result<(ReplayedBlocks, u64), ReceiptError> {
    let bytes =
        std::fs::read(path).map_err(|e| ReceiptError::Io(format!("{}: {e}", path.display())))?;
    let mut blocks = ReplayedBlocks::new();
    let mut max_seq = 0u64;
    for line in String::from_utf8_lossy(&bytes).lines() {
        if line.trim().is_empty() {
            continue;
        }
        if let Ok(r) = serde_json::from_str::<Receipt>(line) {
            max_seq = max_seq.max(r.seq);
            if r.kind == ReceiptKind::Blocked {
                blocks.push((r.proposal_id.clone(), r.worker_id.clone()));
            }
            if r.kind == ReceiptKind::Unblocked {
                blocks.retain(|(p, w)| !(p == &r.proposal_id && w == &r.worker_id));
            }
        }
    }
    Ok((blocks, max_seq))
}

/// Mint this process's gate epoch: malkuth-owned, caller fields never enter.
#[must_use]
pub fn boot_epoch() -> String {
    let now = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_nanos())
        .unwrap_or(0);
    format!("g{now}-{}", std::process::id())
}

#[cfg(test)]
mod tests {
    use super::*;

    fn receipt(kind: ReceiptKind, proposal: &str) -> Receipt {
        Receipt {
            seq: 0,
            epoch: "e".into(),
            kind,
            proposal_id: proposal.into(),
            worker_id: "chest".into(),
            action: None,
            code: None,
            at: 0,
        }
    }

    /// A receipt with every identity field under the test's control (the
    /// reconciliation keys on epoch + proposal + worker + action).
    fn full_receipt(
        kind: ReceiptKind,
        epoch: &str,
        seq: u64,
        proposal: &str,
        worker: &str,
        action: Option<&str>,
    ) -> Receipt {
        Receipt {
            seq,
            epoch: epoch.into(),
            kind,
            proposal_id: proposal.into(),
            worker_id: worker.into(),
            action: action.map(Into::into),
            code: None,
            at: 0,
        }
    }

    fn temp_path(tag: &str) -> PathBuf {
        let nanos = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .map(|d| d.as_nanos())
            .unwrap_or(0);
        std::env::temp_dir().join(format!(
            "malkuth-gate-jrnl-{}-{tag}-{nanos}.jsonl",
            std::process::id()
        ))
    }

    #[test]
    fn the_memory_sink_numbers_receipts_monotonically() {
        let sink = MemoryReceiptSink::new();
        let a = sink.append(&receipt(ReceiptKind::Pending, "p1")).unwrap();
        let b = sink.append(&receipt(ReceiptKind::Done, "p1")).unwrap();
        assert_eq!(b.seq, a.seq + 1);
        assert_eq!(sink.receipts().len(), 2);
    }

    #[test]
    fn the_failing_sink_records_the_attempt_and_vetoes() {
        let sink = FailingReceiptSink::new();
        assert!(sink.append(&receipt(ReceiptKind::Pending, "p1")).is_err());
        assert_eq!(
            sink.attempts().len(),
            1,
            "the attempt is visible, nothing persisted"
        );
    }

    #[test]
    fn the_file_journal_round_trips_and_continues_its_sequence() {
        let path = temp_path("roundtrip");
        {
            let journal = FileReceiptJournal::open(&path).unwrap();
            let a = journal
                .append(&receipt(ReceiptKind::Pending, "p1"))
                .unwrap();
            let b = journal.append(&receipt(ReceiptKind::Done, "p1")).unwrap();
            assert_eq!(b.seq, a.seq + 1);
        }
        // A fresh open (the restarted process) continues the numbering and
        // reads the whole history back.
        let journal = FileReceiptJournal::open(&path).unwrap();
        let next = journal
            .append(&receipt(ReceiptKind::Blocked, "p2"))
            .unwrap();
        assert_eq!(next.seq, 3);
        let all = journal.read_all().unwrap();
        assert_eq!(all.len(), 3);
        assert_eq!(all[2].kind, ReceiptKind::Blocked);
        let _ = std::fs::remove_file(&path);
    }

    #[test]
    fn replay_extracts_blocks_and_applies_unblocks_in_order() {
        let path = temp_path("replay");
        let journal = FileReceiptJournal::open(&path).unwrap();
        journal
            .append(&receipt(ReceiptKind::Blocked, "p1"))
            .unwrap();
        journal
            .append(&receipt(ReceiptKind::Blocked, "p2"))
            .unwrap();
        journal
            .append(&receipt(ReceiptKind::Unblocked, "p1"))
            .unwrap();
        let (blocks, seq) = replay_blocks(&path).unwrap();
        assert_eq!(seq, 3);
        assert_eq!(blocks, vec![("p2".to_string(), "chest".to_string())]);
        let _ = std::fs::remove_file(&path);
    }

    #[test]
    fn a_truncated_line_is_skipped_and_counted_not_fatal() {
        let path = temp_path("truncated");
        // One COMPLETE receipt line (as `append` writes it: sinks number from
        // 1, so the stored line carries seq 1) + one truncated tail: the good
        // line parses, the stub is counted as corrupt.
        let mut good = receipt(ReceiptKind::Blocked, "p1");
        good.seq = 1;
        let good = serde_json::to_string(&good).unwrap();
        std::fs::write(&path, format!("{good}\n{{\"seq\":2,\"epo")).unwrap();
        let journal = FileReceiptJournal::open(&path).unwrap();
        assert_eq!(journal.corrupt_lines(), 1);
        assert_eq!(journal.read_all().unwrap().len(), 1);
        // The sequence continues from the last GOOD line.
        let ack = journal
            .append(&receipt(ReceiptKind::Pending, "p9"))
            .unwrap();
        assert_eq!(ack.seq, 2);
        let _ = std::fs::remove_file(&path);
    }
    // ── P2: reconciliation ─────────────────────────────────

    #[test]
    fn reconciliation_lists_sequence_gaps_per_epoch() {
        let receipts = vec![
            full_receipt(ReceiptKind::Pending, "e1", 1, "p", "chest", Some("restart")),
            full_receipt(ReceiptKind::Done, "e1", 2, "p", "chest", Some("restart")),
            // A failed append claimed 3 and wrote nothing: 4 arrives with 3 gone.
            full_receipt(ReceiptKind::Pending, "e1", 4, "p", "chest", Some("restart")),
            full_receipt(ReceiptKind::Done, "e1", 5, "p", "chest", Some("restart")),
            // A new epoch restarts the numbering at 1: no phantom gap.
            full_receipt(ReceiptKind::Blocked, "e2", 1, "q", "chest", None),
        ];
        let report = reconcile(&receipts);
        assert_eq!(
            report.seq_gaps,
            vec![("e1".to_string(), 2, 4)],
            "the missing 3 is listed exactly once, with the numbers on both sides"
        );
        assert!(report.pending_without_done.is_empty() && report.done_without_pending.is_empty());
    }

    #[test]
    fn reconciliation_lists_a_first_line_above_one() {
        let receipts = vec![full_receipt(
            ReceiptKind::Blocked,
            "e",
            7,
            "q",
            "chest",
            None,
        )];
        let report = reconcile(&receipts);
        assert_eq!(report.seq_gaps, vec![("e".to_string(), 0, 7)]);
    }

    #[test]
    fn reconciliation_lists_spent_batches_without_completion() {
        // 已花未做: batch A spent and left open (the crash window), batch B
        // spent and completed. Only A is owed a done.
        let receipts = vec![
            full_receipt(ReceiptKind::Pending, "e", 1, "pa", "chest", Some("restart")),
            full_receipt(ReceiptKind::Pending, "e", 2, "pb", "chest", Some("drain")),
            full_receipt(ReceiptKind::Done, "e", 3, "pb", "chest", Some("drain")),
        ];
        let report = reconcile(&receipts);
        assert_eq!(
            report.pending_without_done,
            vec![(
                "e".to_string(),
                "pa".to_string(),
                "chest".to_string(),
                "restart".to_string()
            )]
        );
        assert!(report.done_without_pending.is_empty());
        assert!(report.seq_gaps.is_empty());
    }

    #[test]
    fn reconciliation_lists_completion_without_a_batch() {
        // A done with no pending before it is tampering or foreign input, never
        // a crash artifact — append-only writing cannot lose the earlier line.
        let receipts = vec![full_receipt(
            ReceiptKind::Done,
            "e",
            1,
            "p",
            "chest",
            Some("drain"),
        )];
        let report = reconcile(&receipts);
        assert_eq!(
            report.done_without_pending,
            vec![(
                "e".to_string(),
                "p".to_string(),
                "chest".to_string(),
                "drain".to_string()
            )]
        );
        assert!(report.pending_without_done.is_empty());
    }

    #[test]
    fn reconciliation_treats_a_second_done_as_an_open_batch_anomaly() {
        // pending + done closes the batch; a SECOND done for the same batch has
        // no open pending left: completion without a batch.
        let receipts = vec![
            full_receipt(ReceiptKind::Pending, "e", 1, "p", "chest", Some("restart")),
            full_receipt(ReceiptKind::Done, "e", 2, "p", "chest", Some("restart")),
            full_receipt(ReceiptKind::Done, "e", 3, "p", "chest", Some("restart")),
        ];
        let report = reconcile(&receipts);
        assert_eq!(report.done_without_pending.len(), 1);
        assert!(report.pending_without_done.is_empty());
    }

    #[test]
    fn reconciliation_of_a_clean_journal_is_empty() {
        let receipts = vec![
            full_receipt(ReceiptKind::Blocked, "e", 1, "q", "chest", None),
            full_receipt(ReceiptKind::Unblocked, "e", 2, "q", "chest", None),
            full_receipt(ReceiptKind::Refused, "e", 3, "p", "chest", Some("drain")),
            full_receipt(ReceiptKind::Pending, "e", 4, "p", "chest", Some("drain")),
            full_receipt(ReceiptKind::Done, "e", 5, "p", "chest", Some("drain")),
        ];
        let report = reconcile(&receipts);
        assert_eq!(
            report,
            Reconciliation {
                seq_gaps: Vec::new(),
                pending_without_done: Vec::new(),
                done_without_pending: Vec::new(),
            },
            "a clean journal lists nothing — the audit is not noise"
        );
    }

    #[test]
    fn reconciliation_distinguishes_epochs_so_a_foreign_epoch_is_its_own_batches() {
        // Two epochs sharing a proposal: each epoch's ledger is its own.
        let receipts = vec![
            full_receipt(ReceiptKind::Pending, "e1", 1, "p", "chest", Some("restart")),
            full_receipt(ReceiptKind::Done, "e2", 1, "p", "chest", Some("restart")),
        ];
        let report = reconcile(&receipts);
        assert_eq!(report.pending_without_done.len(), 1);
        assert_eq!(report.done_without_pending.len(), 1);
    }
}
