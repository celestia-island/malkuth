//! Filesystem-backed [`CoordinationLock`] (POSIX advisory `flock`, unix).
//!
//! One lock file per `key` under `root`. The flock call is blocking, so it is
//! offloaded via [`tokio::task::spawn_blocking`].

use std::{fs::OpenOptions, io, os::fd::AsRawFd, path::PathBuf, time::Duration};

use async_trait::async_trait;

use crate::traits::{CoordinationLock, LockError, LockGuard};

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
    file: Option<std::fs::File>,
    path: PathBuf,
}

#[async_trait]
impl LockGuard for FileGuard {
    async fn release(&mut self) {
        if let Some(file) = self.file.take() {
            // SAFETY: fd is valid and owned by `file`.
            unsafe {
                libc::flock(file.as_raw_fd(), libc::LOCK_UN);
            }
            let _ = std::fs::remove_file(&self.path);
        }
    }
}

impl Drop for FileGuard {
    fn drop(&mut self) {
        // Best-effort release if `release()` wasn't called explicitly.
        if let Some(file) = self.file.take() {
            unsafe {
                libc::flock(file.as_raw_fd(), libc::LOCK_UN);
            }
            let _ = std::fs::remove_file(&self.path);
        }
    }
}

#[async_trait]
impl CoordinationLock for FileLock {
    async fn acquire(&self, key: &str, _lease: Duration) -> Result<Box<dyn LockGuard>, LockError> {
        let root = self.root.clone();
        let path = root.join(sanitize(key));
        let key_owned = key.to_owned();
        let result = tokio::task::spawn_blocking(move || -> Result<FileGuard, LockError> {
            std::fs::create_dir_all(&root)?;
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
                let err = io::Error::last_os_error();
                // EAGAIN / EWOULDBLOCK ⇒ contended; anything else ⇒ io error.
                if matches!(err.raw_os_error(), Some(libc::EAGAIN)) {
                    return Err(LockError::Contended(format!(
                        "flock on '{key_owned}' is held by another live process"
                    )));
                }
                return Err(LockError::Io(err));
            }
            Ok(FileGuard {
                file: Some(file),
                path,
            })
        })
        .await
        .map_err(|e| LockError::Io(io::Error::other(format!("blocking task failed: {e}"))))?;
        match result {
            Ok(g) => Ok(Box::new(g)),
            Err(e) => Err(e),
        }
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

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn sanitize_basic() {
        assert_eq!(sanitize("hello"), "hello.lock");
        assert_eq!(sanitize(""), "default.lock");
        assert_eq!(sanitize("a/b c"), "a_b_c.lock");
    }
}
