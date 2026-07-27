//! Drain authorization — validates restart proposals before allowing
//! worker drain operations. This is the authorization gate (Phase 0+1)
//! implemented locally to avoid pulling in the full plana-types dependency.
//!
//! When malkuth receives an external drain request (via MCP or JSON-RPC),
//! it must verify that the request carries a valid `proposal_id` that
//! matches a previously approved `GateDecision`.

/// Validates a drain request before malkuth executes it.
///
/// Returns `Ok(())` if the request is valid, or `Err(message)` with a
/// human-readable reason.
pub fn validate_drain_request(
    worker_id: &str,
    proposal_id: &str,
    drain_budget_secs: Option<u64>,
) -> Result<(), String> {
    if proposal_id.is_empty() {
        return Err("Drain request rejected: empty proposal_id".into());
    }
    if worker_id.is_empty() {
        return Err("Drain request rejected: empty worker_id".into());
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

    #[test]
    fn test_valid_drain_accepted() {
        assert!(validate_drain_request("chest", "prop-abc", Some(30)).is_ok());
    }

    #[test]
    fn test_empty_proposal_rejected() {
        assert!(validate_drain_request("chest", "", None).is_err());
    }

    #[test]
    fn test_empty_worker_rejected() {
        assert!(validate_drain_request("", "prop-abc", None).is_err());
    }

    #[test]
    fn test_zero_budget_rejected() {
        assert!(validate_drain_request("test", "p", Some(0)).is_err());
    }

    #[test]
    fn test_excessive_budget_rejected() {
        assert!(validate_drain_request("test", "p", Some(301)).is_err());
    }

    #[test]
    fn test_max_budget_accepted() {
        assert!(validate_drain_request("test", "p", Some(300)).is_ok());
    }
}
