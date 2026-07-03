//! Lease-based [`CoordinationLock`] — a file lease with TTL auto-expiry.
//!
//! Unlike the kernel `flock` backend ([`crate::FileLock`]), the liveness signal
//! here is the **lease TTL**, not a kernel lock: the holder renews the lease
//! periodically, and if it crashes *or* stops heartbeating, the lease expires
//! and another owner may acquire it. This is the primitive Subsystem B
//! (leader/follower) uses as its election lease.
//!
//! The renew loop runs on a [`tokio::task::spawn_blocking`] thread.

use std::io;
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};
use std::time::{Duration, SystemTime, UNIX_EPOCH};

use async_trait::async_trait;
use serde::{Deserialize, Serialize};

use crate::traits::{CoordinationLock, LockError, LockGuard};

/// Filesystem lease lock. One lease file per `key` under `root`.
pub struct LeaseLock {
    root: PathBuf,
}

impl LeaseLock {
    #[must_use]
    pub fn new(root: impl Into<PathBuf>) -> Self {
        Self { root: root.into() }
    }
}

#[derive(Serialize, Deserialize)]
struct LeaseRecord {
    owner: String,
    /// Expiry as milliseconds since the Unix epoch.
    expires_at_ms: u64,
}

struct LeaseGuard {
    path: PathBuf,
    owner: String,
    stop: Arc<AtomicBool>,
    handle: Option<std::thread::JoinHandle<()>>,
}

#[async_trait]
impl LockGuard for LeaseGuard {
    async fn release(&mut self) {
        self.stop.store(true, Ordering::Release);
        if let Some(h) = self.handle.take() {
            let _ = h.join();
        }
        // Remove the lease only if we still own it.
        if let Ok(content) = std::fs::read(&self.path) {
            if let Ok(rec) = serde_json::from_slice::<LeaseRecord>(&content) {
                if rec.owner == self.owner {
                    let _ = std::fs::remove_file(&self.path);
                }
            }
        }
    }
}

impl Drop for LeaseGuard {
    fn drop(&mut self) {
        self.stop.store(true, Ordering::Release);
        if let Some(h) = self.handle.take() {
            let _ = h.join();
        }
        // Clean up the lease file if we still own it (covers abandoned/panicked
        // guards, not just an explicit release()).
        if let Ok(content) = std::fs::read(&self.path) {
            if let Ok(rec) = serde_json::from_slice::<LeaseRecord>(&content) {
                if rec.owner == self.owner {
                    let _ = std::fs::remove_file(&self.path);
                }
            }
        }
    }
}

fn now_ms() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_millis() as u64)
        .unwrap_or(0)
}

fn next_owner() -> String {
    use std::sync::atomic::{AtomicU64, Ordering};
    static COUNTER: AtomicU64 = AtomicU64::new(0);
    format!(
        "pid{}-{}-{}",
        std::process::id(),
        COUNTER.fetch_add(1, Ordering::Relaxed),
        now_ms()
    )
}

#[async_trait]
impl CoordinationLock for LeaseLock {
    async fn acquire(&self, key: &str, lease: Duration) -> Result<Box<dyn LockGuard>, LockError> {
        let root = self.root.clone();
        let key_msg = key.to_string();
        let path = root.join(sanitize(key));
        let ttl = lease;
        // Best-effort, NON-blocking acquire: try once. (A retry loop on a
        // detached thread would outlive a dropped `acquire` future and could
        // spuriously win the lease later — so we don't.) `lease` is the TTL
        // written into the lease record, not a wait duration.
        let result = tokio::task::spawn_blocking(move || -> Result<LeaseGuard, LockError> {
            std::fs::create_dir_all(&root)?;
            let owner = next_owner();
            if try_take(&path, &owner, ttl)? {
                let stop = Arc::new(AtomicBool::new(false));
                let stop_c = Arc::clone(&stop);
                let renew_path = path.clone();
                let renew_owner = owner.clone();
                let renew_ttl = ttl;
                let handle = std::thread::spawn(move || {
                    let step = renew_ttl.max(Duration::from_secs(1)) / 3;
                    while !stop_c.load(Ordering::Acquire) {
                        let mut waited = Duration::ZERO;
                        while waited < step {
                            if stop_c.load(Ordering::Acquire) {
                                return;
                            }
                            std::thread::sleep(Duration::from_millis(50));
                            waited += Duration::from_millis(50);
                        }
                        let rec = LeaseRecord {
                            owner: renew_owner.clone(),
                            expires_at_ms: now_ms() + renew_ttl.as_millis() as u64,
                        };
                        let _ = write_atomic(&renew_path, &rec);
                    }
                });
                return Ok(LeaseGuard {
                    path,
                    owner,
                    stop,
                    handle: Some(handle),
                });
            }
            Err(LockError::Contended(format!(
                "lease on '{}' held by another live owner",
                key_msg
            )))
        })
        .await
        .map_err(|e| LockError::Io(io::Error::other(format!("blocking task failed: {e}"))))?;
        match result {
            Ok(g) => Ok(Box::new(g)),
            Err(e) => Err(e),
        }
    }
}

/// Try to claim the lease at `path` for `owner` with `ttl`. Returns true if we
/// now own it. CAS via atomic temp-file rename; re-read to confirm we won.
fn try_take(path: &Path, owner: &str, ttl: Duration) -> Result<bool, LockError> {
    if let Ok(content) = std::fs::read(path) {
        if let Ok(rec) = serde_json::from_slice::<LeaseRecord>(&content) {
            if rec.expires_at_ms > now_ms() && rec.owner != owner {
                // Live lease held by someone else.
                return Ok(false);
            }
        }
    }
    // Expired or absent — (re)claim it.
    let rec = LeaseRecord {
        owner: owner.to_string(),
        expires_at_ms: now_ms() + ttl.as_millis() as u64,
    };
    write_atomic(path, &rec)?;
    // Confirm we won the race.
    if let Ok(content) = std::fs::read(path) {
        if let Ok(rec) = serde_json::from_slice::<LeaseRecord>(&content) {
            return Ok(rec.owner == owner);
        }
    }
    Ok(false)
}

/// Atomic write: temp file in the same dir, then rename over the target.
fn write_atomic(path: &Path, rec: &LeaseRecord) -> Result<(), LockError> {
    let data =
        serde_json::to_vec(rec).map_err(|e| io::Error::new(io::ErrorKind::InvalidData, e))?;
    let tmp = path.with_extension("tmp");
    std::fs::write(&tmp, &data)?;
    std::fs::rename(&tmp, path)?;
    Ok(())
}

fn sanitize(key: &str) -> String {
    let mut out = String::with_capacity(key.len());
    for c in key.chars() {
        if c.is_ascii_alphanumeric() || c == '-' || c == '_' {
            out.push(c);
        } else {
            out.push('_');
        }
    }
    if out.is_empty() {
        out.push_str("default");
    }
    out.push_str(".lease");
    out
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::traits::CoordinationLock;

    #[tokio::test]
    async fn acquire_renew_release() {
        let dir = tempdir();
        let lock = LeaseLock::new(&dir);
        let mut g = lock
            .acquire("device-a", Duration::from_secs(2))
            .await
            .unwrap();
        // A second acquirer is contended while the lease is live.
        let r = tokio::time::timeout(
            Duration::from_millis(300),
            lock.acquire("device-a", Duration::from_secs(2)),
        )
        .await;
        assert!(
            r.is_err() || r.unwrap().is_err(),
            "second acquire should be contended"
        );
        g.release().await;
        // After release, the lease file is gone — a new owner can take it.
        let g2 = lock
            .acquire("device-a", Duration::from_secs(2))
            .await
            .unwrap();
        drop(g2);
    }

    fn tempdir() -> PathBuf {
        let p =
            std::env::temp_dir().join(format!("malkuth-lease-{}-{}", std::process::id(), now_ms()));
        std::fs::create_dir_all(&p).unwrap();
        p
    }
}
