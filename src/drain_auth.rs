//! Drain authorization — validates restart proposals before allowing
//! worker drain operations. This closes the authorization gate (Phase 0+1)
//! by maintaining a registry of approved proposals.
//!
//! # Flow
//!
//! 1. An external auditor or a human operator decides a restart proposal is
//!    allowed (the upstream audit tooling itself is out of scope here).
//! 2. The deciding side records the outcome via [`ApprovalRegistry::approve`],
//!    passing the exact `proposal_id`, `worker_id`, and the one action the
//!    approval authorizes. Since P0b, a spendable decision (`allow` or
//!    `review` — both mean "a human already looked", see ruling B1) must
//!    carry a verified [`GrantRecord`]: the scepter-side issuer's ed25519
//!    signature over the worker's live `(pid, starttime)` identity. A `block`
//!    (refuse this pair) or an `unblock` (lift exactly that refusal) authorizes
//!    no action and needs no grant.
//! 3. malkuth receives a DrainRequest → `validate_drain_request()` checks
//!    that a live approval entry exists whose stored `proposal_id` AND
//!    `worker_id` both match the request exactly, that it was issued for the
//!    drain action, and — when the entry carries a grant — that the live
//!    worker identity still equals the identity the signature vouches for.
//! 4. Unknown, expired, blocked, wrong-action, or identity-mismatched
//!    proposals are rejected.
//!
//! # Security guarantee
//!
//! Without an approval entry whose `proposal_id` and `worker_id` match the
//! request exactly and whose action is the one being performed, no drain can
//! proceed. Empty or fabricated `proposal_id` values are rejected at the gate:
//! an approval recorded for one proposal never vouches for a different or
//! replayed id, and an approval issued for a restart never vouches for a
//! drain. A grant additionally pins the worker to the exact process
//! incarnation the issuer saw, so the approval dies with that incarnation.
//!
//! The registry records what it is given; the attestation REQUIREMENT (no
//! spendable entry without a verified grant) is enforced at the MCP boundary
//! in `mcp.rs`, which is where the live fleet identity is resolvable.

use crate::gate_grant::GrantRecord;
use std::sync::RwLock;
use std::time::{Duration, SystemTime, UNIX_EPOCH};

/// Cross-node clock tolerance for a grant's `issued_at`: a stamp from the
/// future beyond this window is refused rather than trusted (mirrors NTP
/// step guards). Past-direction skew is bounded by the registry TTL.
const GRANT_CLOCK_SKEW_SECS: u64 = 60;

/// Gate decision for a restart proposal.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum GateDecision {
    Allow,
    Review,
    Block,
    /// Lift a block on this pair. Like `Block` it authorizes nothing and
    /// needs no grant — it only removes the block. A spendable decision on a
    /// blocked pair is refused, not a silent unblock: the operator must
    /// explicitly say `unblock`.
    Unblock,
}

/// The one action an approval authorizes. A grant names its action at
/// registration and can only be spent on that action: a drain approval must
/// never vouch for a restart (and vice versa).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum GateAction {
    Restart,
    Drain,
}

impl GateAction {
    /// The wire name used in the canonical grant message.
    #[must_use]
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Restart => "restart",
            Self::Drain => "drain",
        }
    }
}

/// An approved restart proposal registered in the authorization gate.
#[derive(Debug, Clone)]
struct ApprovalEntry {
    proposal_id: String,
    worker_id: String,
    action: GateAction,
    decision: GateDecision,
    approved_at: SystemTime,
    /// The verified grant this approval was issued with, when it carries one.
    /// Present for every spendable decision registered through the MCP
    /// boundary; the identity check at spend time compares against this.
    grant: Option<GrantRecord>,
    /// Set when the approval has been spent on an action. A consumed entry is
    /// kept (not dropped) so a replay reports `approval_consumed` instead of the
    /// indistinguishable `approval_required`.
    consumed_at: Option<SystemTime>,
}

/// What the registry says about one `(proposal_id, worker_id)` pair.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ApprovalStatus {
    /// Live and not yet spent.
    Approved,
    /// Registered, but older than the registry TTL.
    Expired,
    /// Already spent on an action (one approval authorizes at most one).
    Consumed,
    /// Deliberately blocked: the operator stopped this pair. The block is
    /// pair-wide and immortal — no later allow/review lifts it; only an
    /// explicit `unblock` does.
    Blocked,
    /// Registered for this pair, but for the other action: a drain approval
    /// presented to the restart gate (or vice versa).
    WrongAction,
    /// Nothing registered for this pair — or the pair was blocked and then
    /// explicitly unblocked (the unblock removes the block, not any approval).
    Unknown,
}

/// Why an action was refused at the gate. Each variant maps to a stable,
/// machine-readable code: callers (the CLI, the platform) branch on the code,
/// never on the human message.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum DrainAuthError {
    /// No `proposal_id` / `worker_id` supplied at all.
    InvalidRequest,
    /// The drain budget is 0 or above the 300 s ceiling.
    InvalidBudget(u64),
    /// The spend's resolved budget exceeds the grant's signed envelope.
    BudgetExceeded {
        /// The envelope the issuer signed into the grant.
        granted: u64,
        /// The budget this spend resolved to.
        requested: u64,
    },
    /// Nothing live is registered for this proposal + worker.
    Required,
    /// The approval outlived the registry TTL.
    Expired,
    /// The approval was already spent on an action.
    Consumed,
    /// An operator blocked this proposal + worker pair.
    Blocked,
    /// The approval was issued for the other action kind.
    ActionMismatch,
    /// The approval's grant does not vouch for the worker identity at hand:
    /// either no grant was presented where one is required, or the live
    /// `(pid, starttime)` differs from the one the signature binds.
    Unattested,
    /// The grant's `issued_at` disagrees with the registry clock beyond the
    /// TTL/skew window — cross-node skew must stay bounded by the gate TTL.
    EpochStale,
    /// The registry is at capacity after purging expired entries. Refused
    /// loudly instead of silently evicting a possibly-live approval.
    RegistryFull,
    /// The receipt journal refused the write, so the spend cannot be
    /// evidenced — the approval is un-spent and the action refused
    /// (`Err ⇒ 拒绝兑现`). Retry once the journal recovers.
    JournalUnavailable,
}

impl DrainAuthError {
    /// The stable code an MCP/CLI caller branches on.
    #[must_use]
    pub fn code(&self) -> &'static str {
        match self {
            Self::InvalidRequest => "invalid_request",
            Self::InvalidBudget(_) => "invalid_budget",
            Self::BudgetExceeded { .. } => "approval_budget_exceeded",
            Self::Required => "approval_required",
            Self::Expired => "approval_expired",
            Self::Consumed => "approval_consumed",
            Self::Blocked => "approval_blocked",
            Self::ActionMismatch => "approval_action_mismatch",
            Self::Unattested => "approval_unattested",
            Self::EpochStale => "approval_epoch_stale",
            Self::RegistryFull => "approval_registry_full",
            Self::JournalUnavailable => "approval_journal_unavailable",
        }
    }
}

impl std::fmt::Display for DrainAuthError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::InvalidRequest => f.write_str("empty proposal_id or worker_id"),
            Self::InvalidBudget(budget) => write!(
                f,
                "drain_budget_secs {budget} is outside the accepted range (1..=300)"
            ),
            Self::BudgetExceeded { granted, requested } => write!(
                f,
                "the resolved spend budget {requested}s exceeds the grant's signed envelope ({granted}s)"
            ),
            Self::Required => f.write_str("no live approval for this proposal and worker"),
            Self::Expired => f.write_str("the approval has expired"),
            Self::Consumed => f.write_str("the approval was already spent on an action"),
            Self::Blocked => f.write_str("an operator blocked this proposal"),
            Self::ActionMismatch => {
                f.write_str("the approval was issued for the other action kind")
            }
            Self::Unattested => f.write_str(
                "the approval grant does not vouch for this worker's live process identity",
            ),
            Self::EpochStale => {
                f.write_str("the grant's issued_at is outside the accepted clock window")
            }
            Self::RegistryFull => {
                f.write_str("the approval registry is full; wait for entries to expire")
            }
            Self::JournalUnavailable => f.write_str(
                "the receipt journal refused the write; the spend is refused and the approval left unspent",
            ),
        }
    }
}

/// Thread-safe registry of approved restart proposals.
///
/// Shared between the drain validation path and the daemon's RPC/MCP
/// handlers that receive approval notifications.
///
/// P1 durability: every spend appends a receipt to the [`ReceiptSink`] and a
/// failed append un-spends the approval and refuses the action (`Err ⇒ 拒绝兑现`);
/// blocks live in their own immortal section — no TTL, no capacity eviction,
/// replayed from the journal at startup (`rehydrate_blocks`) and lifted only
/// by an explicit `unblock`.
pub struct ApprovalRegistry {
    entries: RwLock<Vec<ApprovalEntry>>,
    /// Blocked pairs. Separate from `entries` because a block is permanent:
    /// TTL purges and capacity pressure never touch it, and a flood of
    /// approvals must not evict one (blocks cap out with an explicit refusal
    /// instead).
    blocks: RwLock<Vec<BlockEntry>>,
    block_cap: usize,
    /// Every spend/block/unblock/refusal is evidenced here; a failed append
    /// vetoes the spend.
    sink: std::sync::Arc<dyn crate::gate_receipt::ReceiptSink>,
    /// Minted by malkuth at construction; receipts carry it so a prior life
    /// of the process is tellable from this one. Never derived from caller
    /// fields.
    epoch: String,
    max_entries: usize,
    ttl: Duration,
}

/// A blocked pair: protection that survives restarts, washes, and floods.
#[derive(Debug, Clone)]
struct BlockEntry {
    proposal_id: String,
    worker_id: String,
}

/// Hard ceiling on immortal blocks. Beyond it a new block is REFUSED (with
/// `approval_registry_full`) rather than evicting an existing one — the
/// operator seeing the refusal is the correct response to a block flood, a
/// silently dropped block is not.
const BLOCK_CAP: usize = 4096;
/// The authorization budget a drain spend resolves to when the caller
/// passes none (the MCP drain path). Deliberately NOT the drain wait
/// timeout: that is operational, this is the number a grant's signed
/// envelope must cover for the spend to be authorized.
const DEFAULT_DRAIN_BUDGET_SECS: u64 = 30;

impl ApprovalRegistry {
    pub fn new(max_entries: usize, ttl: Duration) -> Self {
        Self::with_sink(
            max_entries,
            ttl,
            std::sync::Arc::new(crate::gate_receipt::MemoryReceiptSink::new()),
            crate::gate_receipt::boot_epoch(),
        )
    }

    /// Full constructor: an explicit sink (the file journal in production,
    /// the memory sink in tests) and an explicit epoch.
    pub fn with_sink(
        max_entries: usize,
        ttl: Duration,
        sink: std::sync::Arc<dyn crate::gate_receipt::ReceiptSink>,
        epoch: String,
    ) -> Self {
        Self {
            entries: RwLock::new(Vec::new()),
            blocks: RwLock::new(Vec::new()),
            block_cap: BLOCK_CAP,
            sink,
            epoch,
            max_entries,
            ttl,
        }
    }

    /// Register an approved proposal. Called after an external auditor or
    /// human confirms the action is authorized. The approval names exactly one
    /// action kind and can only be spent on that action.
    ///
    /// Since P0b a spendable decision (`allow`/`review`) is expected to carry
    /// its verified [`GrantRecord`]; enforcement of that expectation happens
    /// at the MCP boundary, the registry records what it is handed. A `block`
    /// needs no grant — it authorizes nothing.
    ///
    /// # Errors
    ///
    /// [`DrainAuthError::EpochStale`] when the grant's `issued_at` is outside
    /// the clock window (future beyond [`GRANT_CLOCK_SKEW_SECS`] or older
    /// than the TTL), [`DrainAuthError::RegistryFull`] when the registry is at
    /// capacity after purging expired entries — a full registry refuses new
    /// approvals rather than silently dropping a possibly-live one.
    pub fn approve(
        &self,
        proposal_id: &str,
        worker_id: &str,
        action: GateAction,
        decision: GateDecision,
        grant: Option<GrantRecord>,
    ) -> Result<(), DrainAuthError> {
        // Blocks outrank everything: a spendable decision on a blocked pair is
        // REFUSED, not a silent lift — the autonomous loop cannot talk itself
        // past an operator's stop. Only an explicit `unblock` clears it, and
        // the refusal is archived (拒绝入档).
        if self.is_blocked(proposal_id, worker_id) && decision != GateDecision::Unblock {
            let _ = self.append_receipt(
                crate::gate_receipt::ReceiptKind::Refused,
                proposal_id,
                worker_id,
                Some(action),
                Some(DrainAuthError::Blocked.code()),
            );
            return Err(DrainAuthError::Blocked);
        }
        match decision {
            GateDecision::Block => return self.block_pair(proposal_id, worker_id),
            GateDecision::Unblock => return self.unblock_pair(proposal_id, worker_id),
            GateDecision::Allow | GateDecision::Review => {}
        }
        let mut entries = self.entries.write().unwrap();
        // Evict expired entries.
        entries.retain(|e| e.is_valid(self.ttl));
        // At capacity, refuse new approvals instead of evicting index 0:
        // silently dropping the oldest entry can kill a live approval (and
        // lets a flood of registrations evict the entries it targets). A
        // same-pair re-approval replaces its own slot below, so it is never
        // blocked here.
        //
        // Precedence note (deliberate): this capacity check runs BEFORE the
        // epoch window below, so a full registry answers `RegistryFull` even
        // for an epoch-stale grant — both are fail-closed; callers must not
        // read `RegistryFull` as evidence about the grant's clock validity.
        let pair_exists = entries
            .iter()
            .any(|e| e.proposal_id == proposal_id && e.worker_id == worker_id);
        if entries.len() >= self.max_entries && !pair_exists {
            return Err(DrainAuthError::RegistryFull);
        }
        // Clock window for grants: cross-node skew is bounded by the gate TTL
        // in the past direction and GRANT_CLOCK_SKEW_SECS in the future one.
        if let Some(g) = grant.as_ref() {
            let now = unix_now();
            let future_bound = now.saturating_add(GRANT_CLOCK_SKEW_SECS);
            let past_bound = g.issued_at.saturating_add(self.ttl.as_secs());
            if g.issued_at > future_bound || past_bound < now {
                return Err(DrainAuthError::EpochStale);
            }
        }
        // A fresh approval replaces any earlier entry for the same pair —
        // including the other action's: the registry keeps one entry per
        // pair. A re-approved proposal starts unspent, and a superseded
        // grant simply disappears rather than lingers (every direction of
        // the replacement stays fail-closed).
        entries.retain(|e| e.proposal_id != proposal_id || e.worker_id != worker_id);
        entries.push(ApprovalEntry {
            proposal_id: proposal_id.into(),
            worker_id: worker_id.into(),
            action,
            decision,
            approved_at: SystemTime::now(),
            grant,
            consumed_at: None,
        });
        Ok(())
    }

    /// Check whether a proposal is approved for a given worker and action. The
    /// stored `proposal_id` must match exactly: an approval recorded for one
    /// proposal must never authorize a different or fabricated id, even for
    /// the same worker.
    ///
    /// This consults the approval entries only — it does NOT check the
    /// operator block section (an approved entry under a block still returns
    /// true here). `status` and `consume` are the authorities: they check
    /// blocks first.
    pub fn is_approved(&self, proposal_id: &str, worker_id: &str, action: GateAction) -> bool {
        let entries = self.entries.read().unwrap();
        entries.iter().any(|e| {
            e.proposal_id == proposal_id
                && e.worker_id == worker_id
                && e.action == action
                && e.decision != GateDecision::Block
                && e.is_valid(self.ttl)
        })
    }

    /// What the registry currently says about one pair for one action (pure:
    /// never consumes).
    #[must_use]
    pub fn status(&self, proposal_id: &str, worker_id: &str, action: GateAction) -> ApprovalStatus {
        // A block outranks any entry for the pair, for BOTH actions, forever
        // (no TTL): the pair-level stop groove is checked first.
        if self.is_blocked(proposal_id, worker_id) {
            return ApprovalStatus::Blocked;
        }
        let entries = self.entries.read().unwrap();
        for entry in entries.iter().rev() {
            if entry.proposal_id != proposal_id || entry.worker_id != worker_id {
                continue;
            }

            if entry.action != action {
                return ApprovalStatus::WrongAction;
            }

            // Defense-in-depth: since P1 no entry with decision==Block can
            // exist (approve routes block/unblock away and block_pair revokes
            // entries), so this branch is unreachable — kept harmless.
            if entry.decision == GateDecision::Block {
                return ApprovalStatus::Blocked;
            }

            if entry.consumed_at.is_some() {
                return ApprovalStatus::Consumed;
            }

            return if entry.is_valid(self.ttl) {
                ApprovalStatus::Approved
            } else {
                ApprovalStatus::Expired
            };
        }
        ApprovalStatus::Unknown
    }

    /// Spend one approval, atomically. Returns `Ok(())` only when a live,
    /// unspent approval existed for exactly this pair AND the requested
    /// action, and — when the entry carries a grant — when the live worker
    /// identity is exactly the `(pid, starttime)` pair the grant's signature
    /// vouches for; every other outcome is an error and consumes nothing.
    pub fn consume(
        &self,
        proposal_id: &str,
        worker_id: &str,
        action: GateAction,
        live_identity: Option<(u64, u64)>,
        spend_budget_secs: Option<u64>,
    ) -> Result<(), DrainAuthError> {
        let mut entries = self.entries.write().unwrap();
        // Blocks first, pair-wide and immortal: a spend against a blocked pair
        // is refused and the refusal archived (拒绝入档). Checked on the
        // `blocks` field directly (disjoint from the `entries` guard) so the
        // check stays inside the same critical section as the spend: a block
        // landing concurrently cannot be raced past.
        let blocked = self
            .blocks
            .read()
            .unwrap()
            .iter()
            .any(|b| b.proposal_id == proposal_id && b.worker_id == worker_id);
        if blocked {
            let refused = crate::gate_receipt::Receipt {
                seq: 0,
                epoch: self.epoch.clone(),
                kind: crate::gate_receipt::ReceiptKind::Refused,
                proposal_id: proposal_id.to_string(),
                worker_id: worker_id.to_string(),
                action: Some(action.as_str().to_string()),
                code: Some(DrainAuthError::Blocked.code().to_string()),
                at: unix_now(),
            };
            // Best-effort: the refusal itself already refuses; losing its
            // receipt to a broken journal is degradation, not fail-open.
            let _ = self.sink.append(&refused);
            return Err(DrainAuthError::Blocked);
        }
        let mut answer = Err(DrainAuthError::Required);

        for entry in entries.iter_mut().rev() {
            if entry.proposal_id != proposal_id || entry.worker_id != worker_id {
                continue;
            }

            if entry.action != action {
                answer = Err(DrainAuthError::ActionMismatch);
                break;
            }

            // Defense-in-depth: unreachable since P1 (no Block entries can be
            // created; see the note in `status`).
            if entry.decision == GateDecision::Block {
                answer = Err(DrainAuthError::Blocked);
                break;
            }

            if entry.consumed_at.is_some() {
                answer = Err(DrainAuthError::Consumed);
                break;
            }

            if !entry.is_valid(self.ttl) {
                answer = Err(DrainAuthError::Expired);
                break;
            }

            // Identity binding: an attested approval only spends on the exact
            // process incarnation the grant names. A respawned worker (new
            // starttime, maybe new pid) or a hand-copied approval for another
            // process fails here without being consumed.
            if let Some(grant) = entry.grant.as_ref() {
                if live_identity != Some((grant.pid, grant.starttime)) {
                    answer = Err(DrainAuthError::Unattested);
                    break;
                }
                // Budget envelope (v2): a signed drain envelope caps what any
                // spend of this approval may resolve to. `budget > 0` marks an
                // enveloped grant (restart grants mint 0 = no envelope, and
                // restart spends pass no budget either); the check fires
                // before the spend is marked, so nothing is consumed.
                if grant.budget > 0 && spend_budget_secs.is_some_and(|spend| spend > grant.budget) {
                    answer = Err(DrainAuthError::BudgetExceeded {
                        granted: grant.budget,
                        requested: spend_budget_secs.unwrap_or_default(),
                    });
                    break;
                }
            }

            entry.consumed_at = Some(SystemTime::now());
            // The spend is only real once its receipt is durable: a failed
            // append un-spends the approval and refuses the action — the
            // caller retries when the journal recovers, nothing was done.
            // (`Err ⇒ 拒绝兑现`; the pending marker is what a restart
            // reconciles as 已花未做 when the process dies before `done`.)
            let receipt = crate::gate_receipt::Receipt {
                seq: 0,
                epoch: self.epoch.clone(),
                kind: crate::gate_receipt::ReceiptKind::Pending,
                proposal_id: proposal_id.to_string(),
                worker_id: worker_id.to_string(),
                action: Some(action.as_str().to_string()),
                code: None,
                at: unix_now(),
            };
            match self.sink.append(&receipt) {
                Ok(_) => answer = Ok(()),
                Err(_) => {
                    entry.consumed_at = None;
                    answer = Err(DrainAuthError::JournalUnavailable);
                }
            }
            break;
        }
        answer
    }

    /// The worker identity a live entry's grant vouches for, for one pair +
    /// action. `None` when there is no live entry, the entry is spent, or it
    /// carries no grant.
    ///
    /// Used for pre-action checks BEFORE a side effect (signalling a restart,
    /// where the registry's `consume` word would come too late); the final
    /// authority is still [`Self::consume`], which re-checks atomically. Like
    /// [`Self::is_approved`], this reads the approval entries only and does not
    /// consult the operator block section.
    #[must_use]
    pub fn attested_identity(
        &self,
        proposal_id: &str,
        worker_id: &str,
        action: GateAction,
    ) -> Option<(u64, u64)> {
        let entries = self.entries.read().unwrap();
        entries
            .iter()
            .rev()
            .find(|e| {
                e.proposal_id == proposal_id && e.worker_id == worker_id && e.action == action
            })
            .filter(|e| e.is_valid(self.ttl) && e.consumed_at.is_none())
            .and_then(|e| e.grant.as_ref().map(|g| (g.pid, g.starttime)))
    }

    /// How long an approval stays live, in seconds (reported by `malkuth_approve`).
    #[must_use]
    pub fn ttl_secs(&self) -> u64 {
        self.ttl.as_secs()
    }

    /// Remove all expired entries.
    pub fn purge(&self) {
        let mut entries = self.entries.write().unwrap();
        entries.retain(|e| e.is_valid(self.ttl));
    }

    /// Is this pair under a block? Blocks are pair-wide (both actions),
    /// immortal until an explicit `unblock`, and never evicted.
    #[must_use]
    pub fn is_blocked(&self, proposal_id: &str, worker_id: &str) -> bool {
        self.blocks
            .read()
            .unwrap()
            .iter()
            .any(|b| b.proposal_id == proposal_id && b.worker_id == worker_id)
    }

    /// Register the pair-wide, immortal block and archive it. The in-memory
    /// insert happens REGARDLESS of journal health — losing protection to a
    /// journal outage would be fail-open. A failed append is still returned
    /// as [`DrainAuthError::JournalUnavailable`] so the caller knows the
    /// block will not survive a restart until the journal recovers.
    fn block_pair(&self, proposal_id: &str, worker_id: &str) -> Result<(), DrainAuthError> {
        // The block revokes any live approval for the pair: in the old model
        // that revocation was a side effect of replacing the entry; here the
        // block lives in its own section, so the revocation is explicit.
        self.entries
            .write()
            .unwrap()
            .retain(|e| !(e.proposal_id == proposal_id && e.worker_id == worker_id));
        let mut blocks = self.blocks.write().unwrap();
        let already = blocks
            .iter()
            .any(|b| b.proposal_id == proposal_id && b.worker_id == worker_id);
        if !already {
            if blocks.len() >= self.block_cap {
                // 拒绝入档: the over-cap refusal is archived like any other
                // refusal (best-effort — the refusal itself already refused).
                let _ = self.append_receipt(
                    crate::gate_receipt::ReceiptKind::Refused,
                    proposal_id,
                    worker_id,
                    None,
                    Some(DrainAuthError::RegistryFull.code()),
                );
                return Err(DrainAuthError::RegistryFull);
            }
            blocks.push(BlockEntry {
                proposal_id: proposal_id.to_string(),
                worker_id: worker_id.to_string(),
            });
        }
        let receipt = crate::gate_receipt::Receipt {
            seq: 0,
            epoch: self.epoch.clone(),
            kind: crate::gate_receipt::ReceiptKind::Blocked,
            proposal_id: proposal_id.to_string(),
            worker_id: worker_id.to_string(),
            action: None,
            code: None,
            at: unix_now(),
        };
        match self.sink.append(&receipt) {
            Ok(_) => Ok(()),
            Err(_) => Err(DrainAuthError::JournalUnavailable),
        }
    }

    /// Lift a block explicitly. Idempotent: unblocking a non-blocked pair
    /// succeeds without appending anything (the journal only records the
    /// lift that actually lifted something).
    ///
    /// The in-memory block is lifted FIRST; the journal append is best-effort.
    /// On append failure the lift stands in memory but the journal still holds
    /// the `blocked` line — a restart replays it and the block RESURRECTS (the
    /// fail-closed direction), while this call answers
    /// [`DrainAuthError::JournalUnavailable`] so the operator knows the lift
    /// is not yet durable.
    fn unblock_pair(&self, proposal_id: &str, worker_id: &str) -> Result<(), DrainAuthError> {
        let removed = {
            let mut blocks = self.blocks.write().unwrap();
            let before = blocks.len();
            blocks.retain(|b| !(b.proposal_id == proposal_id && b.worker_id == worker_id));
            before != blocks.len()
        };
        if !removed {
            return Ok(());
        }
        let receipt = crate::gate_receipt::Receipt {
            seq: 0,
            epoch: self.epoch.clone(),
            kind: crate::gate_receipt::ReceiptKind::Unblocked,
            proposal_id: proposal_id.to_string(),
            worker_id: worker_id.to_string(),
            action: None,
            code: None,
            at: unix_now(),
        };
        match self.sink.append(&receipt) {
            Ok(_) => Ok(()),
            Err(_) => Err(DrainAuthError::JournalUnavailable),
        }
    }

    /// Append a receipt through the sink (helper for paths that hold no other
    /// `self` borrow; the spend path calls `sink.append` directly to stay
    /// field-disjoint from the entries guard).
    fn append_receipt(
        &self,
        kind: crate::gate_receipt::ReceiptKind,
        proposal_id: &str,
        worker_id: &str,
        action: Option<GateAction>,
        code: Option<&str>,
    ) -> Result<crate::gate_receipt::ReceiptAck, crate::gate_receipt::ReceiptError> {
        let receipt = crate::gate_receipt::Receipt {
            seq: 0,
            epoch: self.epoch.clone(),
            kind,
            proposal_id: proposal_id.to_string(),
            worker_id: worker_id.to_string(),
            action: action.map(|a| a.as_str().to_string()),
            code: code.map(str::to_string),
            at: unix_now(),
        };
        self.sink.append(&receipt)
    }

    /// Record that a spent action's synchronous acceptance completed (the
    /// pending marker now has its `done` pair). Best-effort: the action is
    /// already out; a failed `done` append leaves the crash window open and
    /// is logged, not fatal.
    pub fn spend_completed(&self, proposal_id: &str, worker_id: &str, action: GateAction) {
        let receipt = crate::gate_receipt::Receipt {
            seq: 0,
            epoch: self.epoch.clone(),
            kind: crate::gate_receipt::ReceiptKind::Done,
            proposal_id: proposal_id.to_string(),
            worker_id: worker_id.to_string(),
            action: Some(action.as_str().to_string()),
            code: None,
            at: unix_now(),
        };
        if let Err(error) = self.sink.append(&receipt) {
            tracing::warn!(
                proposal_id,
                worker_id,
                action = action.as_str(),
                %error,
                "gate receipt `done` append failed; the spend stays pending in the journal"
            );
        }
    }

    /// Insert replayed blocks (startup rehydration). Idempotent per pair;
    /// over the block cap it refuses the remainder loudly — replay never
    /// evicts.
    pub fn rehydrate_blocks(&self, pairs: &[(String, String)]) -> usize {
        let mut blocks = self.blocks.write().unwrap();
        let mut inserted = 0usize;
        for (proposal_id, worker_id) in pairs {
            let already = blocks
                .iter()
                .any(|b| &b.proposal_id == proposal_id && &b.worker_id == worker_id);
            if already {
                continue;
            }
            if blocks.len() >= self.block_cap {
                tracing::warn!(
                    proposal_id,
                    worker_id,
                    block_cap = self.block_cap,
                    "replayed block dropped: the block section is at capacity (replay never evicts)"
                );
                continue;
            }
            blocks.push(BlockEntry {
                proposal_id: proposal_id.clone(),
                worker_id: worker_id.clone(),
            });
            inserted += 1;
        }
        inserted
    }

    /// The minted epoch receipts from this registry carry.
    #[must_use]
    pub fn epoch(&self) -> &str {
        &self.epoch
    }
}

impl Default for ApprovalRegistry {
    fn default() -> Self {
        Self::new(100, Duration::from_secs(300))
    }
}

impl ApprovalEntry {
    fn is_valid(&self, ttl: Duration) -> bool {
        SystemTime::now()
            .duration_since(self.approved_at)
            .map(|elapsed| elapsed < ttl)
            .unwrap_or(false)
    }
}

/// Validates a drain request against the authorization gate **and spends the
/// approval on success**.
///
/// The spend is what makes "one approval authorizes one action" true: a failure
/// never consumes, and a second call with the same proposal answers
/// [`DrainAuthError::Consumed`] instead of acting twice. When the approval
/// carries a grant, `live_identity` must be the worker's current
/// `(pid, starttime)` snapshot — anything else (including `None`, an
/// unsupervised worker) answers [`DrainAuthError::Unattested`].
///
/// # Errors
///
/// Returns the matching [`DrainAuthError`] when the request is malformed, the
/// budget is out of range, or the approval is missing / expired / already
/// spent / identity-mismatched.
pub fn validate_drain_request(
    worker_id: &str,
    proposal_id: &str,
    drain_budget_secs: Option<u64>,
    live_identity: Option<(u64, u64)>,
    registry: &ApprovalRegistry,
) -> Result<(), DrainAuthError> {
    if proposal_id.trim().is_empty() || worker_id.trim().is_empty() {
        return Err(DrainAuthError::InvalidRequest);
    }

    if let Some(budget) = drain_budget_secs {
        if budget == 0 || budget > 300 {
            return Err(DrainAuthError::InvalidBudget(budget));
        }
    }

    // The spend's authorization budget (v2): an explicit library budget is
    // checked against the grant's signed envelope; `None` resolves to the
    // service default — the MCP drain path never passes one, so its effective
    // authorization budget is this constant (distinct from the drain WAIT
    // timeout, which is operational and never authorized).
    let spend_budget = drain_budget_secs.or(Some(DEFAULT_DRAIN_BUDGET_SECS));
    registry.consume(
        proposal_id,
        worker_id,
        GateAction::Drain,
        live_identity,
        spend_budget,
    )
}

fn unix_now() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_secs())
        .unwrap_or(0)
}

/// The process-wide approval registry every MCP tool handler shares.
///
/// malkuth's approvals live in-process (there is no second store to drift
/// from), so one registry per server process is the whole contract. Defaults:
/// 100 entries, 300 s TTL.
///
/// Durability is deployment-wired: `MALKUTH_GATE_JOURNAL` names a JSONL file
/// and the registry replays its blocks at construction (`rehydrate_blocks`)
/// and appends every spend/block/refusal there — a spend whose receipt cannot
/// be persisted refuses the action. Without the variable the registry falls
/// back to the in-memory sink (nothing survives a restart; the pre-P1
/// behavior, kept for tests and embedders that have not adopted the journal).
#[must_use]
pub fn global() -> &'static ApprovalRegistry {
    static REGISTRY: std::sync::OnceLock<ApprovalRegistry> = std::sync::OnceLock::new();
    REGISTRY.get_or_init(|| {
        let configured = std::env::var("MALKUTH_GATE_JOURNAL")
            .ok()
            .map(|p| p.trim().to_string())
            .filter(|p| !p.is_empty());
        registry_from_journal_path(configured.as_deref())
    })
}

/// The startup assembly `global()` runs: a file-backed registry with its
/// journal replayed when a path is configured, the in-memory sink otherwise.
/// Split out of the `OnceLock` so the deployment wiring has a testable seam —
/// process-global env vars cannot be set safely from parallel tests, but the
/// function this feeds can be called directly.
fn registry_from_journal_path(path: Option<&str>) -> ApprovalRegistry {
    path.and_then(|path| match crate::gate_receipt::FileReceiptJournal::open(path) {
            Ok(journal) => {
                let (blocks, _) = crate::gate_receipt::replay_blocks(journal.path()).unwrap_or_else(
                    |error| {
                        tracing::warn!(
                            journal = %path,
                            %error,
                            "block replay failed; operator blocks will NOT be restored this boot"
                        );
                        Default::default()
                    },
                );
                let registry = ApprovalRegistry::with_sink(
                    100,
                    Duration::from_secs(300),
                    std::sync::Arc::new(journal),
                    crate::gate_receipt::boot_epoch(),
                );
                let rehydrated = registry.rehydrate_blocks(&blocks);
                tracing::info!(
                    journal = %path,
                    rehydrated_blocks = rehydrated,
                    "gate receipt journal wired; blocks replayed"
                );
                Some(registry)
            }
        Err(error) => {
            tracing::error!(
                journal = %path,
                %error,
                "gate journal could not be opened; falling back to the memory sink; gate receipts will NOT survive a restart"
            );
            None
        }
    })
    .unwrap_or_else(|| ApprovalRegistry::new(100, Duration::from_secs(300)))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn registry() -> ApprovalRegistry {
        ApprovalRegistry::new(10, Duration::from_secs(300))
    }

    #[test]
    fn test_approved_proposal_is_valid() {
        let reg = registry();
        reg.approve(
            "prop-abc",
            "chest",
            GateAction::Drain,
            GateDecision::Allow,
            None,
        )
        .unwrap();
        assert!(validate_drain_request("chest", "prop-abc", Some(30), None, &reg).is_ok());
    }

    #[test]
    fn test_unapproved_proposal_is_rejected() {
        let reg = registry();
        assert!(validate_drain_request("chest", "prop-unknown", None, None, &reg).is_err());
    }

    #[test]
    fn test_empty_proposal_rejected() {
        let reg = registry();
        assert!(validate_drain_request("chest", "", None, None, &reg).is_err());
    }

    #[test]
    fn test_empty_worker_rejected() {
        let reg = registry();
        assert!(validate_drain_request("", "prop-abc", None, None, &reg).is_err());
    }

    #[test]
    fn an_approval_authorizes_at_most_one_action() {
        let reg = registry();
        reg.approve(
            "prop-once",
            "chest",
            GateAction::Drain,
            GateDecision::Allow,
            None,
        )
        .unwrap();
        assert!(validate_drain_request("chest", "prop-once", Some(30), None, &reg).is_ok());
        let replay = validate_drain_request("chest", "prop-once", Some(30), None, &reg)
            .expect_err("a spent approval must not authorize a second action");
        assert_eq!(replay.code(), "approval_consumed");
        assert_eq!(
            reg.status("prop-once", "chest", GateAction::Drain),
            ApprovalStatus::Consumed
        );
    }

    #[test]
    fn a_failed_validation_does_not_spend_the_approval() {
        let reg = registry();
        reg.approve(
            "prop-keep",
            "chest",
            GateAction::Drain,
            GateDecision::Allow,
            None,
        )
        .unwrap();
        // Too-large budget: the request is refused before the spend.
        let refused = validate_drain_request("chest", "prop-keep", Some(999), None, &reg)
            .expect_err("out-of-range budget");
        assert_eq!(refused.code(), "invalid_budget");
        assert_eq!(
            reg.status("prop-keep", "chest", GateAction::Drain),
            ApprovalStatus::Approved
        );
        // The approval is still whole, so a well-formed request succeeds.
        assert!(validate_drain_request("chest", "prop-keep", Some(30), None, &reg).is_ok());
    }

    #[test]
    fn an_expired_approval_is_refused_with_its_own_code() {
        let reg = ApprovalRegistry::new(10, Duration::from_millis(0));
        reg.approve(
            "prop-old",
            "chest",
            GateAction::Drain,
            GateDecision::Allow,
            None,
        )
        .unwrap();
        std::thread::sleep(Duration::from_millis(2));
        let expired = validate_drain_request("chest", "prop-old", None, None, &reg)
            .expect_err("an expired approval must not authorize anything");
        assert_eq!(expired.code(), "approval_expired");
        assert_eq!(
            reg.status("prop-old", "chest", GateAction::Drain),
            ApprovalStatus::Expired
        );
    }

    #[test]
    fn an_approval_is_bound_to_its_worker() {
        let reg = registry();
        reg.approve(
            "prop-bound",
            "chest",
            GateAction::Drain,
            GateDecision::Allow,
            None,
        )
        .unwrap();
        let mismatch = validate_drain_request("other-worker", "prop-bound", None, None, &reg)
            .expect_err("an approval for one worker must not vouch for another");
        assert_eq!(mismatch.code(), "approval_required");
        // …and the mismatch did not spend the real worker's approval.
        assert_eq!(
            reg.status("prop-bound", "chest", GateAction::Drain),
            ApprovalStatus::Approved
        );
    }

    #[test]
    fn a_block_refuses_a_later_allow() {
        let reg = registry();
        reg.approve(
            "prop-flip",
            "chest",
            GateAction::Drain,
            GateDecision::Allow,
            None,
        )
        .unwrap();
        assert_eq!(
            reg.status("prop-flip", "chest", GateAction::Drain),
            ApprovalStatus::Approved
        );

        // The operator changes their mind: blocking must revoke the allow…
        reg.approve(
            "prop-flip",
            "chest",
            GateAction::Drain,
            GateDecision::Block,
            None,
        )
        .unwrap();
        assert_eq!(
            reg.status("prop-flip", "chest", GateAction::Drain),
            ApprovalStatus::Blocked
        );
        // …and the action must be refused with the block's own code.
        let blocked = validate_drain_request("chest", "prop-flip", None, None, &reg)
            .expect_err("a blocked proposal must not authorize anything");
        assert_eq!(blocked.code(), "approval_blocked");

        // P1: un-blocking is NO LONGER just another approval — a fresh allow
        // on a blocked pair is refused (an autonomous loop must not be able
        // to talk itself past the operator's stop); only an explicit
        // `unblock` lifts it.
        let re_allow = reg.approve(
            "prop-flip",
            "chest",
            GateAction::Drain,
            GateDecision::Allow,
            None,
        );
        assert_eq!(re_allow.unwrap_err().code(), "approval_blocked");
        assert_eq!(
            reg.status("prop-flip", "chest", GateAction::Drain),
            ApprovalStatus::Blocked
        );

        // The explicit unblock lifts the block; the pair is clean (no live
        // approval), so a fresh allow registers and spends normally again.
        reg.approve(
            "prop-flip",
            "chest",
            GateAction::Drain,
            GateDecision::Unblock,
            None,
        )
        .unwrap();
        assert_eq!(
            reg.status("prop-flip", "chest", GateAction::Drain),
            ApprovalStatus::Unknown
        );
        reg.approve(
            "prop-flip",
            "chest",
            GateAction::Drain,
            GateDecision::Allow,
            None,
        )
        .unwrap();
        assert!(validate_drain_request("chest", "prop-flip", None, None, &reg).is_ok());
    }

    #[test]
    fn re_approving_a_proposal_starts_unspent() {
        let reg = registry();
        reg.approve(
            "prop-again",
            "chest",
            GateAction::Drain,
            GateDecision::Allow,
            None,
        )
        .unwrap();
        assert!(validate_drain_request("chest", "prop-again", None, None, &reg).is_ok());
        reg.approve(
            "prop-again",
            "chest",
            GateAction::Drain,
            GateDecision::Allow,
            None,
        )
        .unwrap();
        assert_eq!(
            reg.status("prop-again", "chest", GateAction::Drain),
            ApprovalStatus::Approved
        );
        assert!(validate_drain_request("chest", "prop-again", None, None, &reg).is_ok());
    }

    #[test]
    fn test_blocked_proposal_not_allowed() {
        let reg = registry();
        reg.approve(
            "prop-blocked",
            "chest",
            GateAction::Drain,
            GateDecision::Block,
            None,
        )
        .unwrap();
        assert!(validate_drain_request("chest", "prop-blocked", None, None, &reg).is_err());
    }

    #[test]
    fn test_zero_budget_rejected() {
        let reg = registry();
        reg.approve("p1", "test", GateAction::Drain, GateDecision::Allow, None)
            .unwrap();
        assert!(validate_drain_request("test", "p1", Some(0), None, &reg).is_err());
    }

    #[test]
    fn test_excessive_budget_rejected() {
        let reg = registry();
        reg.approve("p2", "test", GateAction::Drain, GateDecision::Allow, None)
            .unwrap();
        assert!(validate_drain_request("test", "p2", Some(301), None, &reg).is_err());
    }

    #[test]
    fn test_max_budget_accepted() {
        let reg = registry();
        reg.approve("p3", "test", GateAction::Drain, GateDecision::Allow, None)
            .unwrap();
        assert!(validate_drain_request("test", "p3", Some(300), None, &reg).is_ok());
    }

    #[test]
    fn test_wrong_worker_rejected() {
        let reg = registry();
        reg.approve("p4", "chest", GateAction::Drain, GateDecision::Allow, None)
            .unwrap();
        assert!(validate_drain_request("evernight", "p4", None, None, &reg).is_err());
    }

    #[test]
    fn test_review_decision_is_valid() {
        let reg = registry();
        reg.approve("p5", "chest", GateAction::Drain, GateDecision::Review, None)
            .unwrap();
        assert!(validate_drain_request("chest", "p5", None, None, &reg).is_ok());
    }

    #[test]
    fn test_purge_removes_expired() {
        let reg = ApprovalRegistry::new(10, Duration::ZERO);
        reg.approve("p6", "chest", GateAction::Drain, GateDecision::Allow, None)
            .unwrap();
        std::thread::sleep(Duration::from_millis(10));
        assert!(validate_drain_request("chest", "p6", None, None, &reg).is_err());
    }

    #[test]
    fn test_forged_proposal_id_rejected() {
        // An approval recorded for `p-abc` must never vouch for another id —
        // not for a fabricated one, not for the same id under another worker.
        let reg = registry();
        reg.approve(
            "p-abc",
            "chest",
            GateAction::Drain,
            GateDecision::Allow,
            None,
        )
        .unwrap();
        assert!(reg.is_approved("p-abc", "chest", GateAction::Drain));
        assert!(!reg.is_approved("p-forged", "chest", GateAction::Drain));
        assert!(validate_drain_request("chest", "p-forged", None, None, &reg).is_err());
        assert!(!reg.is_approved("p-abc", "evernight", GateAction::Drain));
    }

    #[test]
    fn an_approval_is_bound_to_its_action() {
        let reg = registry();
        // Issued for a restart, presented to the drain gate: refused with its
        // own code, and the refusal spends nothing.
        reg.approve(
            "prop-act",
            "chest",
            GateAction::Restart,
            GateDecision::Allow,
            None,
        )
        .unwrap();
        assert_eq!(
            reg.status("prop-act", "chest", GateAction::Drain),
            ApprovalStatus::WrongAction
        );
        let mismatch = validate_drain_request("chest", "prop-act", None, None, &reg)
            .expect_err("a restart approval must not authorize a drain");
        assert_eq!(mismatch.code(), "approval_action_mismatch");
        // The approval is still whole for its own action.
        assert!(
            reg.consume("prop-act", "chest", GateAction::Restart, None, None)
                .is_ok()
        );
    }

    // ── P0b: grant binding ──────────────────────────────────

    fn grant_for(pid: u64, starttime: u64, issued_at: u64) -> GrantRecord {
        GrantRecord {
            pid,
            starttime,
            issued_at,
            budget: 0,
            digest: "0".repeat(64),
            sig: "0".repeat(128),
        }
    }

    fn unix_now_test() -> u64 {
        SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .map(|d| d.as_secs())
            .unwrap_or(0)
    }

    #[test]
    fn a_granted_approval_spends_only_on_its_identity() {
        let reg = registry();
        let grant = grant_for(4242, 987_654, unix_now_test());
        reg.approve(
            "prop-ident",
            "chest",
            GateAction::Drain,
            GateDecision::Allow,
            Some(grant),
        )
        .unwrap();

        // The exact live identity the grant names: spendable.
        assert!(
            validate_drain_request("chest", "prop-ident", Some(30), Some((4242, 987_654)), &reg)
                .is_ok()
        );

        // …and only once: replay answers approval_consumed.
        let replay =
            validate_drain_request("chest", "prop-ident", Some(30), Some((4242, 987_654)), &reg)
                .expect_err("identity match on a spent approval is still a replay");
        assert_eq!(replay.code(), "approval_consumed");
    }

    #[test]
    fn a_spend_above_the_granted_envelope_refuses_and_lefts_it_unspent() {
        let reg = registry();
        let mut grant = grant_for(4242, 987_654, unix_now_test());
        grant.budget = 10;
        reg.approve(
            "prop-cap",
            "chest",
            GateAction::Drain,
            GateDecision::Allow,
            Some(grant),
        )
        .unwrap();
        // The library budget 30 resolves fine by itself, but the grant's
        // signed envelope is 10: the spend refuses and the approval stays
        // spendable (nothing was consumed).
        let over =
            validate_drain_request("chest", "prop-cap", Some(30), Some((4242, 987_654)), &reg)
                .expect_err("a 30s spend must not pass a 10s envelope");
        assert_eq!(over.code(), "approval_budget_exceeded");
        assert_eq!(
            reg.status("prop-cap", "chest", GateAction::Drain),
            ApprovalStatus::Approved,
            "an envelope refusal must not consume the approval"
        );
        // …and within the envelope the same approval spends.
        assert!(
            validate_drain_request("chest", "prop-cap", Some(10), Some((4242, 987_654)), &reg)
                .is_ok()
        );
    }

    #[test]
    fn the_resolved_default_refuses_an_envelope_below_thirty() {
        // The MCP path passes no explicit budget: it resolves to the service
        // default 30. A grant enveloped BELOW that default therefore refuses
        // the default spend too — resolving `None` must never skip the
        // comparison, only fill it in.
        let reg = registry();
        let mut grant = grant_for(4242, 987_654, unix_now_test());
        grant.budget = 10;
        reg.approve(
            "prop-low",
            "chest",
            GateAction::Drain,
            GateDecision::Allow,
            Some(grant),
        )
        .unwrap();
        let refused = validate_drain_request("chest", "prop-low", None, Some((4242, 987_654)), &reg)
            .expect_err("the resolved default 30 must exceed a 10s envelope");
        assert_eq!(refused.code(), "approval_budget_exceeded");
        assert_eq!(
            reg.status("prop-low", "chest", GateAction::Drain),
            ApprovalStatus::Approved,
            "the default-budget refusal must not consume the approval"
        );
    }

    #[test]
    fn the_mcp_default_budget_resolves_against_the_envelope() {
        let reg = registry();
        let mut grant = grant_for(4242, 987_654, unix_now_test());
        grant.budget = 30;
        reg.approve(
            "prop-mcp",
            "chest",
            GateAction::Drain,
            GateDecision::Allow,
            Some(grant),
        )
        .unwrap();
        // The MCP drain path passes no explicit budget: it resolves to the
        // service default (30) and must exactly meet a 30s envelope.
        assert!(
            validate_drain_request("chest", "prop-mcp", None, Some((4242, 987_654)), &reg).is_ok()
        );
    }

    #[test]
    fn an_envelopeless_grant_never_binds_a_restart_spend() {
        let reg = registry();
        // Restart grants mint budget 0 (no per-spend envelope) — a restart
        // spend passes no budget either, so nothing is compared.
        let mut grant = grant_for(4242, 987_654, unix_now_test());
        grant.budget = 0;
        reg.approve(
            "prop-rs",
            "chest",
            GateAction::Restart,
            GateDecision::Allow,
            Some(grant),
        )
        .unwrap();
        assert!(
            reg.consume(
                "prop-rs",
                "chest",
                GateAction::Restart,
                Some((4242, 987_654)),
                None
            )
            .is_ok()
        );
    }

    #[test]
    fn a_wrong_live_identity_refuses_without_spending() {
        let reg = registry();
        let grant = grant_for(4242, 987_654, unix_now_test());
        reg.approve(
            "prop-mis",
            "chest",
            GateAction::Drain,
            GateDecision::Allow,
            Some(grant),
        )
        .unwrap();

        // Respawned worker: same pid, new starttime.
        let respawned =
            validate_drain_request("chest", "prop-mis", Some(30), Some((4242, 987_655)), &reg)
                .expect_err("a respawned worker is not the attested incarnation");
        assert_eq!(respawned.code(), "approval_unattested");

        // Different pid entirely.
        let other_pid =
            validate_drain_request("chest", "prop-mis", Some(30), Some((4243, 987_654)), &reg)
                .expect_err("another pid is not the attested incarnation");
        assert_eq!(other_pid.code(), "approval_unattested");

        // Unsupervised worker (no live snapshot at all).
        let unsupervised = validate_drain_request("chest", "prop-mis", Some(30), None, &reg)
            .expect_err("no live identity means the attestation cannot hold");
        assert_eq!(unsupervised.code(), "approval_unattested");

        // All refusals spent nothing: the right identity still spends.
        assert!(
            validate_drain_request("chest", "prop-mis", Some(30), Some((4242, 987_654)), &reg)
                .is_ok()
        );
    }

    #[test]
    fn attested_identity_exposes_the_binding_for_pre_checks() {
        let reg = registry();
        assert_eq!(
            reg.attested_identity("prop-pre", "chest", GateAction::Restart),
            None
        );
        let grant = grant_for(7, 8, unix_now_test());
        reg.approve(
            "prop-pre",
            "chest",
            GateAction::Restart,
            GateDecision::Allow,
            Some(grant),
        )
        .unwrap();
        assert_eq!(
            reg.attested_identity("prop-pre", "chest", GateAction::Restart),
            Some((7, 8))
        );
        // Grantless entry (pre-P0b shape, registry-internal registrations):
        // no identity to enforce, consume stays permissive for them.
        reg.approve(
            "prop-plain",
            "chest",
            GateAction::Restart,
            GateDecision::Allow,
            None,
        )
        .unwrap();
        assert_eq!(
            reg.attested_identity("prop-plain", "chest", GateAction::Restart),
            None
        );
    }

    #[test]
    fn a_grant_from_the_far_future_is_epoch_stale() {
        let reg = registry();
        let future = grant_for(1, 2, unix_now_test() + GRANT_CLOCK_SKEW_SECS + 5);
        let refused = reg
            .approve(
                "prop-fut",
                "chest",
                GateAction::Drain,
                GateDecision::Allow,
                Some(future),
            )
            .expect_err("a grant stamped beyond the skew window must not register");
        assert_eq!(refused.code(), "approval_epoch_stale");
        assert_eq!(
            reg.status("prop-fut", "chest", GateAction::Drain),
            ApprovalStatus::Unknown
        );
    }

    #[test]
    fn a_grant_older_than_the_ttl_is_epoch_stale() {
        let reg = registry();
        let past = grant_for(1, 2, unix_now_test().saturating_sub(301));
        let refused = reg
            .approve(
                "prop-past",
                "chest",
                GateAction::Drain,
                GateDecision::Allow,
                Some(past),
            )
            .expect_err("a grant older than the TTL must not register");
        assert_eq!(refused.code(), "approval_epoch_stale");
    }

    #[test]
    fn a_grant_within_the_clock_window_registers() {
        // The skew window itself is part of the frozen contract (60 s of
        // issuer/gate clock drift, 300 s TTL): pin the exact constant so a
        // change can never ride in silently behind "some window".
        assert_eq!(GRANT_CLOCK_SKEW_SECS, 60);

        let reg = registry();
        let now = unix_now_test();
        // Freshly issued, and issued 100 s ago (within the 300 s TTL, beyond
        // the 60 s skew): both must register. Issued AT the future bound
        // (`now + skew`) and just inside it (`now + 1`) must also register —
        // refusal is strictly `> now + skew`, and time only moves forward
        // between this stamp and the check, which can only shrink the gap.
        // The past bound has NO mirror: forward time movement enlarges the
        // past-bound violation (`issued_at + ttl < now_later`), so a stamp
        // exactly TTL old can go stale between this clock read and the
        // registry's own. Only an inward margin (1 s inside the 300 s TTL)
        // registers deterministically; 1 s past the bound is pinned as a
        // refusal by `a_grant_older_than_the_ttl_is_epoch_stale`.
        for (label, issued_at) in [
            ("now", now),
            ("recent", now - 100),
            ("past-inside-ttl", now - 299),
            ("future-at-bound", now + GRANT_CLOCK_SKEW_SECS),
            ("future-inside-skew", now + 1),
        ] {
            reg.approve(
                &format!("prop-{label}"),
                "chest",
                GateAction::Drain,
                GateDecision::Allow,
                Some(grant_for(1, 2, issued_at)),
            )
            .unwrap_or_else(|e| panic!("{label} grant must register: {e}"));
            assert_eq!(
                reg.status(&format!("prop-{label}"), "chest", GateAction::Drain),
                ApprovalStatus::Approved
            );
        }
    }

    #[test]
    fn a_full_registry_refuses_instead_of_evicting() {
        let reg = ApprovalRegistry::new(1, Duration::from_secs(300));
        reg.approve(
            "prop-a",
            "chest",
            GateAction::Drain,
            GateDecision::Allow,
            None,
        )
        .unwrap();
        let refused = reg
            .approve(
                "prop-b",
                "evernight",
                GateAction::Drain,
                GateDecision::Allow,
                None,
            )
            .expect_err("a full registry must refuse instead of silently evicting");
        assert_eq!(refused.code(), "approval_registry_full");
        // The first entry survived the refused registration attempt.
        assert_eq!(
            reg.status("prop-a", "chest", GateAction::Drain),
            ApprovalStatus::Approved
        );
        // Same-pair re-approval replaces its own slot even at capacity.
        reg.approve(
            "prop-a",
            "chest",
            GateAction::Drain,
            GateDecision::Block,
            None,
        )
        .unwrap();
        assert_eq!(
            reg.status("prop-a", "chest", GateAction::Drain),
            ApprovalStatus::Blocked
        );
    }

    #[test]
    fn a_full_registry_recovers_when_entries_expire() {
        let reg = ApprovalRegistry::new(1, Duration::from_millis(10));
        reg.approve(
            "prop-old",
            "chest",
            GateAction::Drain,
            GateDecision::Allow,
            None,
        )
        .unwrap();
        let full = reg
            .approve(
                "prop-new",
                "other",
                GateAction::Drain,
                GateDecision::Allow,
                None,
            )
            .expect_err("registry of one live entry must refuse a second worker");
        assert_eq!(full.code(), "approval_registry_full");
        std::thread::sleep(Duration::from_millis(20));
        reg.approve(
            "prop-new",
            "other",
            GateAction::Drain,
            GateDecision::Allow,
            None,
        )
        .expect("after the first entry expired the slot is free again");
    }

    // ── P1 durability: the receipt sink vetoes, blocks are immortal ──

    use crate::gate_receipt::{FailingReceiptSink, MemoryReceiptSink, ReceiptKind, ReceiptSink};

    fn temp_journal_path(tag: &str) -> std::path::PathBuf {
        let nanos = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .map(|d| d.as_nanos())
            .unwrap_or(0);
        std::env::temp_dir().join(format!(
            "malkuth-gate-journal-{}-{tag}-{nanos}",
            std::process::id()
        ))
    }

    fn memory_sink() -> std::sync::Arc<MemoryReceiptSink> {
        std::sync::Arc::new(MemoryReceiptSink::new())
    }

    #[test]
    fn a_journal_failure_refuses_the_spend_and_leaves_it_unspent() {
        // `Err ⇒ 拒绝兑现`: the sink's veto un-spends the approval, so the
        // caller can retry once the journal recovers — nothing was done and
        // nothing was lost.
        let reg = ApprovalRegistry::with_sink(
            10,
            Duration::from_secs(300),
            std::sync::Arc::new(FailingReceiptSink::new()),
            "epoch-test".into(),
        );
        reg.approve(
            "prop-j",
            "chest",
            GateAction::Drain,
            GateDecision::Allow,
            None,
        )
        .unwrap();
        let refused = validate_drain_request("chest", "prop-j", None, None, &reg)
            .expect_err("a broken journal must veto the spend");
        assert_eq!(refused.code(), "approval_journal_unavailable");
        assert_eq!(
            reg.status("prop-j", "chest", GateAction::Drain),
            ApprovalStatus::Approved,
            "the spend must be rolled back when the receipt cannot be persisted"
        );
    }

    #[test]
    fn a_block_refuses_a_later_allow_and_archives_the_refusal() {
        let sink = memory_sink();
        let reg =
            ApprovalRegistry::with_sink(10, Duration::from_secs(300), sink.clone(), "e".into());
        reg.approve(
            "prop-b",
            "chest",
            GateAction::Drain,
            GateDecision::Block,
            None,
        )
        .unwrap();
        // A fresh allow does NOT lift the block — it is refused outright.
        let re_allowed = reg.approve(
            "prop-b",
            "chest",
            GateAction::Drain,
            GateDecision::Allow,
            None,
        );
        assert_eq!(re_allowed.unwrap_err().code(), "approval_blocked");
        // …and a spend against the block refuses too, with the refusal in the
        // journal (拒绝入档).
        let refused = validate_drain_request("chest", "prop-b", None, None, &reg)
            .expect_err("a blocked pair must not authorize anything");
        assert_eq!(refused.code(), "approval_blocked");
        let archived = sink.receipts();
        assert!(
            archived.iter().any(|r| r.kind == ReceiptKind::Refused
                && r.proposal_id == "prop-b"
                && r.code.as_deref() == Some("approval_blocked")),
            "the refusal must be archived: {archived:?}"
        );
    }

    #[test]
    fn blocks_survive_a_restart_through_the_journal() {
        // 重启后 block 仍在: the block is written to the journal; a FRESH
        // registry (the restarted process) replays it and refuses again.
        let path = temp_journal_path("replay");
        let first = ApprovalRegistry::with_sink(
            10,
            Duration::from_secs(300),
            std::sync::Arc::new(crate::gate_receipt::FileReceiptJournal::open(&path).unwrap()),
            "epoch-a".into(),
        );
        first
            .approve(
                "prop-r",
                "chest",
                GateAction::Drain,
                GateDecision::Block,
                None,
            )
            .unwrap();

        // Restarted-process construction: replay the journal FIRST (this is
        // the startup assembly `drain_auth::global()` performs), then answer.
        let journal = crate::gate_receipt::FileReceiptJournal::open(&path).unwrap();
        let (blocks, _) = crate::gate_receipt::replay_blocks(journal.path()).unwrap();
        let restarted = ApprovalRegistry::with_sink(
            10,
            Duration::from_secs(300),
            std::sync::Arc::new(journal),
            "epoch-b".into(),
        );
        restarted.rehydrate_blocks(&blocks);
        assert_eq!(
            restarted.status("prop-r", "chest", GateAction::Drain),
            ApprovalStatus::Blocked,
            "the replayed block must be live in the restarted registry"
        );
        assert!(
            validate_drain_request("chest", "prop-r", None, None, &restarted).is_err(),
            "the replayed block must refuse the spend"
        );
        let _ = std::fs::remove_file(&path);
    }

    #[test]
    fn a_flood_of_approvals_never_evicts_blocks() {
        // 洪水不驱逐: approvals are capped and refuse when full; the block,
        // which lives outside that capacity, keeps refusing regardless.
        let reg = registry();
        reg.approve(
            "prop-flood",
            "chest",
            GateAction::Drain,
            GateDecision::Block,
            None,
        )
        .unwrap();
        for i in 0..20 {
            let outcome = reg.approve(
                &format!("prop-fill-{i}"),
                &format!("worker-{i}"),
                GateAction::Drain,
                GateDecision::Allow,
                None,
            );
            if let Err(error) = outcome {
                assert_eq!(error.code(), "approval_registry_full", "iteration {i}");
            }
        }
        assert_eq!(
            reg.status("prop-flood", "chest", GateAction::Drain),
            ApprovalStatus::Blocked
        );
        assert!(
            validate_drain_request("chest", "prop-flood", None, None, &reg).is_err(),
            "the flood must not have lifted the block"
        );
    }

    #[test]
    fn the_configured_journal_is_wired_and_replayed() {
        // The deployment seam `drain_auth::global()` feeds: a configured path
        // must produce a registry whose block section was replayed from that
        // journal; no configuration must produce a clean memory registry.
        let path = temp_journal_path("wiring");
        {
            let journal = crate::gate_receipt::FileReceiptJournal::open(&path).unwrap();
            journal
                .append(&crate::gate_receipt::Receipt {
                    seq: 0,
                    epoch: "seed".into(),
                    kind: ReceiptKind::Blocked,
                    proposal_id: "prop-wire".into(),
                    worker_id: "chest".into(),
                    action: None,
                    code: None,
                    at: 0,
                })
                .unwrap();
        }
        let wired = registry_from_journal_path(Some(path.to_str().unwrap()));
        assert_eq!(
            wired.status("prop-wire", "chest", GateAction::Drain),
            ApprovalStatus::Blocked,
            "the wiring must replay operator blocks into the live registry"
        );
        let plain = registry_from_journal_path(None);
        assert_eq!(
            plain.status("prop-wire", "chest", GateAction::Drain),
            ApprovalStatus::Unknown,
            "without a configured journal there is nothing to replay"
        );
        // A path that cannot open (here: the empty path) must fall back to the
        // memory registry loudly — never create stray files, never refuse the
        // gate outright. (Blank/whitespace configs are filtered to None by
        // `global()` before they reach this seam.)
        let fallback = registry_from_journal_path(Some(""));
        assert_eq!(
            fallback.status("prop-wire", "chest", GateAction::Drain),
            ApprovalStatus::Unknown
        );
        let _ = std::fs::remove_file(&path);
    }

    #[test]
    fn the_unblock_lifts_only_an_explicit_pair() {
        let reg = registry();
        reg.approve(
            "prop-u",
            "chest",
            GateAction::Drain,
            GateDecision::Block,
            None,
        )
        .unwrap();
        reg.approve(
            "prop-u",
            "other",
            GateAction::Drain,
            GateDecision::Block,
            None,
        )
        .unwrap();
        reg.approve(
            "prop-u",
            "chest",
            GateAction::Drain,
            GateDecision::Unblock,
            None,
        )
        .unwrap();
        assert_eq!(
            reg.status("prop-u", "chest", GateAction::Drain),
            ApprovalStatus::Unknown,
            "the unblocked pair must be clean"
        );
        assert_eq!(
            reg.status("prop-u", "other", GateAction::Drain),
            ApprovalStatus::Blocked,
            "the sibling pair's block must be untouched"
        );
    }

    #[test]
    fn the_block_section_refuses_at_capacity_and_never_evicts() {
        // README claim pinned: the 4097th distinct pair is REFUSED
        // (`approval_registry_full`) and every earlier block still refuses its
        // spend — capacity pressure never lifts protection.
        let sink = memory_sink();
        let reg =
            ApprovalRegistry::with_sink(10, Duration::from_secs(300), sink.clone(), "e".into());
        for i in 0..4096 {
            let outcome = reg.approve(
                &format!("prop-cap-{i}"),
                "chest",
                GateAction::Drain,
                GateDecision::Block,
                None,
            );
            assert!(outcome.is_ok(), "block {i} must register: {outcome:?}");
        }
        let over = reg.approve(
            "prop-cap-4096",
            "chest",
            GateAction::Drain,
            GateDecision::Block,
            None,
        );
        assert_eq!(over.unwrap_err().code(), "approval_registry_full");
        // The FIRST block is as immortal as ever after the section filled.
        assert_eq!(
            reg.status("prop-cap-0", "chest", GateAction::Drain),
            ApprovalStatus::Blocked
        );
        // Over-cap REFUSAL is archived like any other refusal.
        assert!(
            sink.receipts()
                .iter()
                .any(|r| r.kind == ReceiptKind::Refused && r.proposal_id == "prop-cap-4096")
        );
    }

    #[test]
    fn a_spend_writes_pending_then_done() {
        let sink = memory_sink();
        let reg =
            ApprovalRegistry::with_sink(10, Duration::from_secs(300), sink.clone(), "e".into());
        reg.approve(
            "prop-p",
            "chest",
            GateAction::Drain,
            GateDecision::Allow,
            None,
        )
        .unwrap();
        validate_drain_request("chest", "prop-p", None, None, &reg).unwrap();
        reg.spend_completed("prop-p", "chest", GateAction::Drain);
        let kinds: Vec<ReceiptKind> = sink.receipts().iter().map(|r| r.kind).collect();
        assert_eq!(kinds, vec![ReceiptKind::Pending, ReceiptKind::Done]);
    }

    #[test]
    fn receipts_carry_the_registry_epoch_never_caller_fields() {
        // 自持纪元: the epoch is minted by malkuth at construction; two
        // registries stamp different epochs even when callers pass identical
        // fields into identical flows.
        let sink_a = memory_sink();
        let sink_b = memory_sink();
        let a = ApprovalRegistry::with_sink(
            10,
            Duration::from_secs(300),
            sink_a.clone(),
            "epoch-a".into(),
        );
        let b = ApprovalRegistry::with_sink(
            10,
            Duration::from_secs(300),
            sink_b.clone(),
            "epoch-b".into(),
        );
        assert_ne!(a.epoch(), b.epoch());
        for reg in [&a, &b] {
            reg.approve(
                "prop-ea",
                "chest",
                GateAction::Drain,
                GateDecision::Allow,
                None,
            )
            .unwrap();
            validate_drain_request("chest", "prop-ea", None, None, reg).unwrap();
        }
        for (sink, epoch) in [(sink_a, "epoch-a"), (sink_b, "epoch-b")] {
            let receipts = sink.receipts();
            assert!(
                receipts.iter().all(|r| r.epoch == epoch),
                "every receipt stamps its own registry's epoch: {receipts:?}"
            );
        }
    }
}
