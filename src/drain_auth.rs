//! Drain authorization — validates restart proposals before allowing
//! worker drain operations. This closes the authorization gate (Phase 0+1)
//! by maintaining a registry of approved proposals.
//!
//! # Flow
//!
//! 1. An external auditor or a human operator decides a restart proposal is
//!    allowed (the upstream audit tooling itself is out of scope here).
//! 2. The deciding side records the outcome via [`ApprovalRegistry::approve`],
//!    passing the exact `proposal_id` and `worker_id`.
//! 3. malkuth receives a DrainRequest → `validate_drain_request()` checks
//!    that a live approval entry exists whose stored `proposal_id` AND
//!    `worker_id` both match the request exactly.
//! 4. Unknown, expired, or blocked proposals are rejected.
//!
//! # Security guarantee
//!
//! Without an approval entry whose `proposal_id` and `worker_id` match the
//! request exactly, no drain can proceed. Empty or fabricated `proposal_id`
//! values are rejected at the gate: an approval recorded for one proposal
//! never vouches for a different or replayed id.

use std::sync::RwLock;
use std::time::{Duration, SystemTime};

/// Gate decision for a restart proposal.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum GateDecision {
    Allow,
    Review,
    Block,
}

/// An approved restart proposal registered in the authorization gate.
#[derive(Debug, Clone)]
struct ApprovalEntry {
    proposal_id: String,
    worker_id: String,
    decision: GateDecision,
    approved_at: SystemTime,
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
    /// human confirms a restart is authorized.
    pub fn approve(&self, proposal_id: &str, worker_id: &str, decision: GateDecision) {
        let mut entries = self.entries.write().unwrap();
        // Evict expired entries.
        entries.retain(|e| e.is_valid(self.ttl));
        if entries.len() >= self.max_entries {
            entries.remove(0);
        }
        // A fresh approval replaces any earlier entry for the same pair: a
        // re-approved proposal starts unspent, and the old spent entry must not
        // shadow it on the next lookup.
        entries.retain(|e| e.proposal_id != proposal_id || e.worker_id != worker_id);
        entries.push(ApprovalEntry {
            proposal_id: proposal_id.into(),
            worker_id: worker_id.into(),
            decision,
            approved_at: SystemTime::now(),
            consumed_at: None,
        });
    }

    /// Check whether a proposal is approved for a given worker. The stored
    /// `proposal_id` must match exactly: an approval recorded for one
    /// proposal must never authorize a different or fabricated id, even for
    /// the same worker.
    pub fn is_approved(&self, proposal_id: &str, worker_id: &str) -> bool {
        let entries = self.entries.read().unwrap();
        entries.iter().any(|e| {
            e.proposal_id == proposal_id
                && e.worker_id == worker_id
                && e.decision != GateDecision::Block
                && e.is_valid(self.ttl)
        })
    }

    /// What the registry currently says about one pair (pure: never consumes).
    #[must_use]
    pub fn status(&self, proposal_id: &str, worker_id: &str) -> ApprovalStatus {
        let entries = self.entries.read().unwrap();
        for entry in entries.iter().rev() {
            if entry.proposal_id != proposal_id || entry.worker_id != worker_id {
                continue;
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
    /// unspent approval existed for exactly this pair; every other outcome is
    /// an error and consumes nothing.
    pub fn consume(&self, proposal_id: &str, worker_id: &str) -> Result<(), DrainAuthError> {
        let mut entries = self.entries.write().unwrap();
        let mut answer = Err(DrainAuthError::Required);

        for entry in entries.iter_mut().rev() {
            if entry.proposal_id != proposal_id || entry.worker_id != worker_id {
                continue;
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

            entry.consumed_at = Some(SystemTime::now());
            answer = Ok(());
            break;
        }
        answer
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
/// [`DrainAuthError::Consumed`] instead of acting twice.
///
/// # Errors
///
/// Returns the matching [`DrainAuthError`] when the request is malformed, the
/// budget is out of range, or the approval is missing / expired / already spent.
pub fn validate_drain_request(
    worker_id: &str,
    proposal_id: &str,
    drain_budget_secs: Option<u64>,
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

    registry.consume(proposal_id, worker_id)
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
        reg.approve("prop-abc", "chest", GateDecision::Allow);
        assert!(validate_drain_request("chest", "prop-abc", Some(30), &reg).is_ok());
    }

    #[test]
    fn test_unapproved_proposal_is_rejected() {
        let reg = registry();
        assert!(validate_drain_request("chest", "prop-unknown", None, &reg).is_err());
    }

    #[test]
    fn test_empty_proposal_rejected() {
        let reg = registry();
        assert!(validate_drain_request("chest", "", None, &reg).is_err());
    }

    #[test]
    fn test_empty_worker_rejected() {
        let reg = registry();
        assert!(validate_drain_request("", "prop-abc", None, &reg).is_err());
    }

    #[test]
    fn an_approval_authorizes_at_most_one_action() {
        let reg = registry();
        reg.approve("prop-once", "chest", GateDecision::Allow);
        assert!(validate_drain_request("chest", "prop-once", Some(30), &reg).is_ok());
        let replay = validate_drain_request("chest", "prop-once", Some(30), &reg)
            .expect_err("a spent approval must not authorize a second action");
        assert_eq!(replay.code(), "approval_consumed");
        assert_eq!(reg.status("prop-once", "chest"), ApprovalStatus::Consumed);
    }

    #[test]
    fn a_failed_validation_does_not_spend_the_approval() {
        let reg = registry();
        reg.approve("prop-keep", "chest", GateDecision::Allow);
        // Too-large budget: the request is refused before the spend.
        let refused = validate_drain_request("chest", "prop-keep", Some(999), &reg)
            .expect_err("out-of-range budget");
        assert_eq!(refused.code(), "invalid_budget");
        assert_eq!(reg.status("prop-keep", "chest"), ApprovalStatus::Approved);
        // The approval is still whole, so a well-formed request succeeds.
        assert!(validate_drain_request("chest", "prop-keep", Some(30), &reg).is_ok());
    }

    #[test]
    fn an_expired_approval_is_refused_with_its_own_code() {
        let reg = ApprovalRegistry::new(10, Duration::from_millis(0));
        reg.approve("prop-old", "chest", GateDecision::Allow);
        std::thread::sleep(Duration::from_millis(2));
        let expired = validate_drain_request("chest", "prop-old", None, &reg)
            .expect_err("an expired approval must not authorize anything");
        assert_eq!(expired.code(), "approval_expired");
        assert_eq!(reg.status("prop-old", "chest"), ApprovalStatus::Expired);
    }

    #[test]
    fn an_approval_is_bound_to_its_worker() {
        let reg = registry();
        reg.approve("prop-bound", "chest", GateDecision::Allow);
        let mismatch = validate_drain_request("other-worker", "prop-bound", None, &reg)
            .expect_err("an approval for one worker must not vouch for another");
        assert_eq!(mismatch.code(), "approval_required");
        // …and the mismatch did not spend the real worker's approval.
        assert_eq!(reg.status("prop-bound", "chest"), ApprovalStatus::Approved);
    }

    #[test]
    fn a_block_overrides_an_earlier_allow() {
        let reg = registry();
        reg.approve("prop-flip", "chest", GateDecision::Allow);
        assert_eq!(reg.status("prop-flip", "chest"), ApprovalStatus::Approved);

        // The operator changes their mind: blocking must revoke the allow…
        reg.approve("prop-flip", "chest", GateDecision::Block);
        assert_eq!(reg.status("prop-flip", "chest"), ApprovalStatus::Blocked);
        // …and the action must be refused with the block's own code.
        let blocked = validate_drain_request("chest", "prop-flip", None, &reg)
            .expect_err("a blocked proposal must not authorize anything");
        assert_eq!(blocked.code(), "approval_blocked");

        // Un-blocking is just another approval: the pair becomes usable again.
        reg.approve("prop-flip", "chest", GateDecision::Allow);
        assert!(validate_drain_request("chest", "prop-flip", None, &reg).is_ok());
    }

    #[test]
    fn re_approving_a_proposal_starts_unspent() {
        let reg = registry();
        reg.approve("prop-again", "chest", GateDecision::Allow);
        assert!(validate_drain_request("chest", "prop-again", None, &reg).is_ok());
        reg.approve("prop-again", "chest", GateDecision::Allow);
        assert_eq!(reg.status("prop-again", "chest"), ApprovalStatus::Approved);
        assert!(validate_drain_request("chest", "prop-again", None, &reg).is_ok());
    }

    #[test]
    fn test_blocked_proposal_not_allowed() {
        let reg = registry();
        reg.approve("prop-blocked", "chest", GateDecision::Block);
        assert!(validate_drain_request("chest", "prop-blocked", None, &reg).is_err());
    }

    #[test]
    fn test_zero_budget_rejected() {
        let reg = registry();
        reg.approve("p1", "test", GateDecision::Allow);
        assert!(validate_drain_request("test", "p1", Some(0), &reg).is_err());
    }

    #[test]
    fn test_excessive_budget_rejected() {
        let reg = registry();
        reg.approve("p2", "test", GateDecision::Allow);
        assert!(validate_drain_request("test", "p2", Some(301), &reg).is_err());
    }

    #[test]
    fn test_max_budget_accepted() {
        let reg = registry();
        reg.approve("p3", "test", GateDecision::Allow);
        assert!(validate_drain_request("test", "p3", Some(300), &reg).is_ok());
    }

    #[test]
    fn test_wrong_worker_rejected() {
        let reg = registry();
        reg.approve("p4", "chest", GateDecision::Allow);
        assert!(validate_drain_request("evernight", "p4", None, &reg).is_err());
    }

    #[test]
    fn test_review_decision_is_valid() {
        let reg = registry();
        reg.approve("p5", "chest", GateDecision::Review);
        assert!(validate_drain_request("chest", "p5", None, &reg).is_ok());
    }

    #[test]
    fn test_purge_removes_expired() {
        let reg = ApprovalRegistry::new(10, Duration::ZERO);
        reg.approve("p6", "chest", GateDecision::Allow);
        std::thread::sleep(Duration::from_millis(10));
        assert!(validate_drain_request("chest", "p6", None, &reg).is_err());
    }

    #[test]
    fn test_forged_proposal_id_rejected() {
        // An approval recorded for `p-abc` must never vouch for another id —
        // not for a fabricated one, not for the same id under another worker.
        let reg = registry();
        reg.approve("p-abc", "chest", GateDecision::Allow);
        assert!(reg.is_approved("p-abc", "chest"));
        assert!(!reg.is_approved("p-forged", "chest"));
        assert!(validate_drain_request("chest", "p-forged", None, &reg).is_err());
        assert!(!reg.is_approved("p-abc", "evernight"));
    }
}
