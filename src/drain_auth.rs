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
        if decision == GateDecision::Block {
            return;
        }
        let mut entries = self.entries.write().unwrap();
        // Evict expired entries.
        entries.retain(|e| e.is_valid(self.ttl));
        if entries.len() >= self.max_entries {
            entries.remove(0);
        }
        entries.push(ApprovalEntry {
            proposal_id: proposal_id.into(),
            worker_id: worker_id.into(),
            decision,
            approved_at: SystemTime::now(),
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

/// Validates a drain request against the authorization gate.
///
/// Returns `Ok(())` if the request passes all checks, or `Err(message)`.
pub fn validate_drain_request(
    worker_id: &str,
    proposal_id: &str,
    drain_budget_secs: Option<u64>,
    registry: &ApprovalRegistry,
) -> Result<(), String> {
    if proposal_id.is_empty() {
        return Err("Drain request rejected: empty proposal_id".into());
    }
    if worker_id.is_empty() {
        return Err("Drain request rejected: empty worker_id".into());
    }
    // ── Authorization gate: verify this exact proposal is approved ──
    if !registry.is_approved(proposal_id, worker_id) {
        return Err(format!(
            "Drain request rejected: proposal_id '{}' is not approved for worker '{}'",
            proposal_id, worker_id
        ));
    }
    if let Some(budget) = drain_budget_secs {
        if budget == 0 {
            return Err("Drain request rejected: drain_budget_secs must be > 0".into());
        }
        if budget > 300 {
            return Err(format!(
                "Drain request rejected: drain_budget_secs {} exceeds max 300s",
                budget
            ));
        }
    }
    Ok(())
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
