//! Filesystem-backed [`CoordinationLock`] (POSIX advisory `flock`, unix).
//!
//! One lock file per `key` under `root`. The flock call is blocking, so it is
//! offloaded via a worker thread — this keeps the backend runtime-agnostic
//! (no `tokio::fs` / `async_std::fs` dependency).

use std::fs::OpenOptions;
use std::io;
use std::os::fd::AsRawFd;
use std::path::{Path, PathBuf};
use std::time::Duration;

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
        // Offload the blocking mkdir + flock to a background thread so we stay
        // runtime-agnostic (works under tokio, async-std, smol).
        let result = blocking_call(move || -> Result<FileGuard, LockError> {
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
                if matches!(err.raw_os_error(), Some(libc::EAGAIN) | Some(libc::EWOULDBLOCK)) {
                    return Err(LockError::Contended(format!(
                        "flock on '{key_owned}' is held by another live process"
                    )));
                }
                return Err(LockError::Io(err));
            }
            Ok(FileGuard { file: Some(file), path })
        })
        .await;
        match result {
            Ok(g) => Ok(Box::new(g)),
            Err(e) => Err(e),
        }
    }
}

fn sanitize(key: &str) -> String {
    let key_short = key;
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
    let _ = key_short;
    out
}

/// Run `f` on a dedicated blocking thread and await its result.
///
/// Implemented with `std::thread::spawn` + a oneshot so the backend needs no
/// runtime dependency. (For hot paths a runtime-native blocking pool would be
/// preferable, but lock acquisition is rare.)
fn blocking_call<F, T>(f: F) -> std::pin::Pin<Box<dyn std::future::Future<Output = T> + Send>>
where
    F: FnOnce() -> T + Send + 'static,
    T: Send + 'static,
{
    use std::sync::mpsc;
    let (tx, rx) = mpsc::channel::<T>();
    std::thread::spawn(move || {
        let v = f();
        let _ = tx.send(v);
    });
    Box::pin(async move {
        match rx.recv() {
            Ok(v) => v,
            Err(_) => panic!("blocking_call worker thread panicked"),
        }
    })
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
