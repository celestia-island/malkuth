//! Subsystem B — leader/follower election backed by the core [`LeaseLock`].
//!
//! A candidate acquires the lease for `key`; while leader, the lease's
//! background thread renews it before the TTL elapses, so leadership survives
//! as long as the process is alive and heartbeating. On crash the lease
//! expires and a follower may promote itself. Fencing comes from the
//! exclusivity of the lease (only one owner at a time).

use std::sync::Mutex;
use std::time::{Duration, SystemTime, UNIX_EPOCH};

use crate::lease::LeaseLock;
use crate::traits::{CoordinationLock, ElectionError, LockGuard};
use crate::{LeaderAnnounce, LeaderElector};
use async_trait::async_trait;

/// Lease-backed leader elector. One per group/device.
pub struct LeaseLeaderElector {
    lock: LeaseLock,
    key: String,
    node_id: String,
    instance_id: String,
    /// Monotonic term, incremented on each acquisition.
    term: Mutex<u64>,
    /// The held lease guard + last announcement (present while we are leader).
    held: Mutex<Option<(Box<dyn LockGuard>, LeaderAnnounce)>>,
}

impl LeaseLeaderElector {
    /// Create an elector. `lock_root` is the directory holding lease files.
    #[must_use]
    pub fn new(
        lock_root: impl Into<std::path::PathBuf>,
        node_id: impl Into<String>,
        instance_id: impl Into<String>,
        key: impl Into<String>,
    ) -> Self {
        Self {
            lock: LeaseLock::new(lock_root),
            key: key.into(),
            node_id: node_id.into(),
            instance_id: instance_id.into(),
            term: Mutex::new(0),
            held: Mutex::new(None),
        }
    }
}

#[async_trait]
impl LeaderElector for LeaseLeaderElector {
    async fn try_acquire(&self, ttl: Duration) -> Result<bool, ElectionError> {
        let guard = self
            .lock
            .acquire(&self.key, ttl)
            .await
            .map_err(|e| ElectionError::Contended(e.to_string()))?;
        let term = {
            let mut t = self.term.lock().expect("term mutex");
            *t += 1;
            *t
        };
        let announce = LeaderAnnounce {
            node_id: self.node_id.clone(),
            leader_instance_id: self.instance_id.clone(),
            term,
            acquired_at: iso_now(),
            lease_ttl_secs: ttl.as_secs().try_into().unwrap_or(u32::MAX),
        };
        *self.held.lock().expect("held mutex") = Some((guard, announce));
        Ok(true)
    }

    async fn renew(&self) -> Result<bool, ElectionError> {
        // The LeaseLock guard renews itself in the background; we are leader as
        // long as we still hold it.
        Ok(self.held.lock().expect("held mutex").is_some())
    }

    async fn current(&self) -> Result<Option<LeaderAnnounce>, ElectionError> {
        Ok(self
            .held
            .lock()
            .expect("held mutex")
            .as_ref()
            .map(|(_, a)| a.clone()))
    }

    async fn resign(&self) -> Result<(), ElectionError> {
        // Drop the mutex guard before awaiting release().
        let taken = self.held.lock().expect("held mutex").take();
        if let Some((mut guard, _)) = taken {
            guard.release().await;
        }
        Ok(())
    }
}

fn iso_now() -> String {
    // Minimal ISO-8601-ish timestamp without a date crate (seconds precision).
    let secs = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_secs())
        .unwrap_or(0);
    format!("epoch:{secs}")
}

#[cfg(test)]
mod tests {
    use super::*;

    fn shared_dir() -> std::path::PathBuf {
        let dir = std::env::temp_dir().join(format!(
            "malkuth-leader-{}-{}",
            std::process::id(),
            std::time::SystemTime::now()
                .duration_since(UNIX_EPOCH)
                .unwrap()
                .as_nanos()
        ));
        std::fs::create_dir_all(&dir).unwrap();
        dir
    }

    #[tokio::test]
    async fn one_leader_at_a_time() {
        let dir = shared_dir();
        let a = LeaseLeaderElector::new(dir.clone(), "device-a", "leader-A", "device-a");
        let b = LeaseLeaderElector::new(dir, "device-a", "leader-B", "device-a");
        assert!(a.try_acquire(Duration::from_secs(2)).await.unwrap());
        // B is contended while A holds the lease.
        let r = tokio::time::timeout(
            Duration::from_millis(300),
            b.try_acquire(Duration::from_secs(2)),
        )
        .await;
        let became = matches!(r, Ok(Ok(true)));
        assert!(!became, "B should not become leader while A holds");
        assert_eq!(
            a.current().await.unwrap().unwrap().leader_instance_id,
            "leader-A"
        );
        a.resign().await.unwrap();
        // Now B can take over.
        assert!(b.try_acquire(Duration::from_secs(2)).await.unwrap());
        assert_eq!(
            b.current().await.unwrap().unwrap().leader_instance_id,
            "leader-B"
        );
    }
}
