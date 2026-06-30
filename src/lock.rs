//! Layer 2 — coordination lock trait with pluggable backends.
//!
//! [`CoordinationLock`] is the single abstraction shared by both fault
//! tolerance strategies: Subsystem A (Replica) uses it to coordinate
//! concurrent writes; Subsystem B (Leader/Follower) uses it as the leader
//! lease. That trait unification is where "the principles are common" lands.
//!
//! Backends (feature-gated):
//! - `file-lock`  — POSIX advisory `flock`, for evernight (JSONL / config).
//! - `pg-lock`    — `pg_advisory_lock`, for entelecheia / shittim-chest (TODO).
//! - `lease`      — file lock with TTL auto-expiry on crash (TODO).

use std::time::Duration;

use async_trait::async_trait;
use thiserror::Error;

/// Errors that can occur while acquiring or holding a lock.
#[derive(Debug, Error)]
pub enum LockError {
    /// The lock is held by another live owner.
    #[error("lock held by another owner: {0}")]
    Contended(String),
    /// An I/O error occurred.
    #[error("io error: {0}")]
    Io(#[from] std::io::Error),
    /// The backend is not built in (feature disabled).
    #[error("lock backend not available: {0}")]
    Unavailable(&'static str),
}

/// A held lock. Dropping or calling [`release`](LockGuard::release) frees it.
#[async_trait]
pub trait LockGuard: Send + Sync {
    /// Release the lock explicitly.
    async fn release(&mut self);
}

/// A coordination lock backend.
#[async_trait]
pub trait CoordinationLock: Send + Sync {
    /// Acquire (or queue for) the lock named `key`, waiting up to `lease`
    /// for ownership. The returned guard frees the lock on release/drop.
    async fn acquire(&self, key: &str, lease: Duration) -> Result<Box<dyn LockGuard>, LockError>;
}

// ═══════════════════════════════════════════════════════════════
// file-lock backend (POSIX advisory flock, unix only)
// ═══════════════════════════════════════════════════════════════

#[cfg(all(unix, feature = "file-lock"))]
mod file_backend {
    use super::{CoordinationLock, LockError, LockGuard};
    use async_trait::async_trait;
    use std::fs::OpenOptions;
    use std::os::fd::AsRawFd;
    use std::path::PathBuf;
    use std::time::Duration;

    /// Filesystem-backed advisory lock. One lock file per `key` under `root`.
    pub struct FileLock {
        root: PathBuf,
    }

    impl FileLock {
        #[must_use]
        pub fn new(root: impl Into<PathBuf>) -> Self {
            Self { root: root.into() }
        }
    }

    struct FileGuard {
        file: std::fs::File,
        path: PathBuf,
    }

    #[async_trait]
    impl LockGuard for FileGuard {
        async fn release(&mut self) {
            // SAFETY: fd is valid and owned by self.file.
            unsafe {
                libc::flock(self.file.as_raw_fd(), libc::LOCK_UN);
            }
            let _ = std::fs::remove_file(&self.path);
        }
    }

    #[async_trait]
    impl CoordinationLock for FileLock {
        async fn acquire(
            &self,
            key: &str,
            _lease: Duration,
        ) -> Result<Box<dyn LockGuard>, LockError> {
            tokio::fs::create_dir_all(&self.root).await?;
            let path = self.root.join(sanitize(key));
            let file = OpenOptions::new()
                .create(true)
                .write(true)
                .truncate(false)
                .open(&path)?;
            let fd = file.as_raw_fd();
            // LOCK_EX | LOCK_NB: non-blocking exclusive advisory lock.
            // SAFETY: fd is a valid open file descriptor.
            let r = unsafe { libc::flock(fd, libc::LOCK_EX | libc::LOCK_NB) };
            if r != 0 {
                return Err(LockError::Contended(format!(
                    "flock on '{key}' is held by another live process"
                )));
            }
            Ok(Box::new(FileGuard { file, path }))
        }
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
        out.push_str(".lock");
        out
    }
}

#[cfg(all(unix, feature = "file-lock"))]
pub use file_backend::FileLock;
