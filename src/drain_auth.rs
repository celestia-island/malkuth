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
//!    is a refusal record and needs no grant.
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
    /// Deliberately blocked: the operator changed their mind. A block overrides
    /// an earlier allow for the same pair.
    Blocked,
    /// Registered for this pair, but for the other action: a drain approval
    /// presented to the restart gate (or vice versa).
    WrongAction,
    /// Nothing was ever registered for this pair (or it was blocked).
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
}

impl DrainAuthError {
    /// The stable code an MCP/CLI caller branches on.
    #[must_use]
    pub fn code(&self) -> &'static str {
        match self {
            Self::InvalidRequest => "invalid_request",
            Self::InvalidBudget(_) => "invalid_budget",
            Self::Required => "approval_required",
            Self::Expired => "approval_expired",
            Self::Consumed => "approval_consumed",
            Self::Blocked => "approval_blocked",
            Self::ActionMismatch => "approval_action_mismatch",
            Self::Unattested => "approval_unattested",
            Self::EpochStale => "approval_epoch_stale",
            Self::RegistryFull => "approval_registry_full",
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
        }
    }
}

/// Thread-safe registry of approved restart proposals.
///
/// Shared between the drain validation path and the daemon's RPC/MCP
/// handlers that receive approval notifications.
pub struct ApprovalRegistry {
    entries: RwLock<Vec<ApprovalEntry>>,
    max_entries: usize,
    ttl: Duration,
}

impl ApprovalRegistry {
    pub fn new(max_entries: usize, ttl: Duration) -> Self {
        Self {
            entries: RwLock::new(Vec::new()),
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
        let mut entries = self.entries.write().unwrap();
        // Evict expired entries.
        entries.retain(|e| e.is_valid(self.ttl));
        // At capacity, refuse new approvals instead of evicting index 0:
        // silently dropping the oldest entry can kill a live approval (and
        // lets a flood of registrations evict the entries it targets). A
        // same-pair re-approval replaces its own slot below, so it is never
        // blocked here.
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
        let entries = self.entries.read().unwrap();
        for entry in entries.iter().rev() {
            if entry.proposal_id != proposal_id || entry.worker_id != worker_id {
                continue;
            }

            if entry.action != action {
                return ApprovalStatus::WrongAction;
            }

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
    ) -> Result<(), DrainAuthError> {
        let mut entries = self.entries.write().unwrap();
        let mut answer = Err(DrainAuthError::Required);

        for entry in entries.iter_mut().rev() {
            if entry.proposal_id != proposal_id || entry.worker_id != worker_id {
                continue;
            }

            if entry.action != action {
                answer = Err(DrainAuthError::ActionMismatch);
                break;
            }

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
            }

            entry.consumed_at = Some(SystemTime::now());
            answer = Ok(());
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
    /// authority is still [`Self::consume`], which re-checks atomically.
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

    registry.consume(proposal_id, worker_id, GateAction::Drain, live_identity)
}

fn unix_now() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_secs())
        .unwrap_or(0)
}

/// The process-wide approval registry every MCP tool handler shares.
///
/// malkuth's approvals live in-process (there is no second store to drift from),
/// so one registry per server process is the whole contract. Defaults: 100
/// entries, 300 s TTL.
#[must_use]
pub fn global() -> &'static ApprovalRegistry {
    static REGISTRY: std::sync::OnceLock<ApprovalRegistry> = std::sync::OnceLock::new();
    REGISTRY.get_or_init(ApprovalRegistry::default)
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
    fn a_block_overrides_an_earlier_allow() {
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

        // Un-blocking is just another approval: the pair becomes usable again.
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
            reg.consume("prop-act", "chest", GateAction::Restart, None)
                .is_ok()
        );
    }

    // ── P0b: grant binding ──────────────────────────────────

    fn grant_for(pid: u64, starttime: u64, issued_at: u64) -> GrantRecord {
        GrantRecord {
            pid,
            starttime,
            issued_at,
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
        let reg = registry();
        let now = unix_now_test();
        // Freshly issued, and issued 100 s ago (within the 300 s TTL, beyond
        // the 60 s skew): both must register.
        reg.approve(
            "prop-now",
            "chest",
            GateAction::Drain,
            GateDecision::Allow,
            Some(grant_for(1, 2, now)),
        )
        .unwrap();
        reg.approve(
            "prop-recent",
            "chest",
            GateAction::Drain,
            GateDecision::Allow,
            Some(grant_for(1, 2, now - 100)),
        )
        .unwrap();
        assert_eq!(
            reg.status("prop-now", "chest", GateAction::Drain),
            ApprovalStatus::Approved
        );
        assert_eq!(
            reg.status("prop-recent", "chest", GateAction::Drain),
            ApprovalStatus::Approved
        );
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
}
