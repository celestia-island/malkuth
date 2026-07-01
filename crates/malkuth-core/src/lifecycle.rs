//! Layer 1 — uniform drain semantics.
//!
//! [`DrainController`] holds the single shared drain/shutdown flag and lets
//! any task wait for it. It is built on [`tokio::sync::Notify`] + atomics.
//!
//! Canonical convention (nginx/Go), still honoured by the signal installer in
//! the `malkuth` crate:
//! - `SIGINT` / `SIGTERM` → graceful drain
//! - `SIGHUP`            → hot config reload (no exit)
//! - `SIGQUIT`           → immediate exit (emergency only)
//!
//! Because drain *triggering* is decoupled from drain *state*, you can also
//! begin a drain from any source — an RPC `Lifecycle.Drain`, a custom
//! [`crate::ExitSource`](crate::hooks::ExitSource), or a plain
//! [`DrainController::begin_drain`] call.

use std::sync::Arc;
use std::sync::atomic::{AtomicBool, AtomicU8, Ordering};

use tokio::sync::Notify;

/// Why the process is stopping (or reloading).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ShutdownKind {
    /// `SIGINT` / `SIGTERM` — drain in-flight work, then exit 0.
    Graceful,
    /// `SIGQUIT` — skip drain, exit fast.
    Immediate,
    /// `SIGHUP` — reload configuration; do NOT exit.
    Reload,
}

impl ShutdownKind {
    const NONE: u8 = 0;
    const GRACEFUL: u8 = 1;
    const IMMEDIATE: u8 = 2;
    const RELOAD: u8 = 3;

    fn to_u8(self) -> u8 {
        match self {
            ShutdownKind::Graceful => Self::GRACEFUL,
            ShutdownKind::Immediate => Self::IMMEDIATE,
            ShutdownKind::Reload => Self::RELOAD,
        }
    }
    fn from_u8(v: u8) -> Option<Self> {
        match v {
            Self::NONE => None,
            Self::GRACEFUL => Some(ShutdownKind::Graceful),
            Self::IMMEDIATE => Some(ShutdownKind::Immediate),
            Self::RELOAD => Some(ShutdownKind::Reload),
            _ => None,
        }
    }

    /// Whether this kind triggers an actual drain (vs. a no-exit reload).
    pub fn causes_drain(self) -> bool {
        matches!(self, ShutdownKind::Graceful | ShutdownKind::Immediate)
    }
}

struct Inner {
    draining: AtomicBool,
    kind: AtomicU8,
    /// Notified whenever drain begins (graceful / immediate).
    drain_notify: Notify,
    /// Notified whenever the kind changes (incl. reload).
    kind_notify: Notify,
}

/// Shared drain/shutdown controller. Cheap to clone (a single `Arc`).
///
/// Trigger drain from anywhere via [`DrainController::begin_drain`]; observe it
/// from the serve loop via [`DrainController::wait_for_drain`]. The signal
/// installer in the `malkuth` crate simply calls `begin_drain` when a signal
/// arrives — but you can also drive it from an RPC or a custom hook.
#[derive(Clone)]
pub struct DrainController {
    inner: Arc<Inner>,
}

impl Default for DrainController {
    fn default() -> Self {
        Self::new()
    }
}

impl DrainController {
    /// Create a fresh controller (nothing draining yet).
    #[must_use]
    pub fn new() -> Self {
        Self {
            inner: Arc::new(Inner {
                draining: AtomicBool::new(false),
                kind: AtomicU8::new(ShutdownKind::NONE),
                drain_notify: Notify::new(),
                kind_notify: Notify::new(),
            }),
        }
    }

    /// Current shutdown kind, if any trigger has fired.
    pub fn kind(&self) -> Option<ShutdownKind> {
        ShutdownKind::from_u8(self.inner.kind.load(Ordering::Acquire))
    }

    /// Whether drain has begun (a graceful/immediate trigger fired, or
    /// [`begin_drain`](Self::begin_drain) was called).
    pub fn is_draining(&self) -> bool {
        self.inner.draining.load(Ordering::Acquire)
    }

    /// Wait until a shutdown/reload signal fires and return its kind.
    ///
    /// Returns immediately if one has already fired. Resolves on reload too —
    /// callers that only care about drain should use [`wait_for_drain`](Self::wait_for_drain).
    pub async fn wait_for_signal(&self) -> ShutdownKind {
        loop {
            if let Some(k) = self.kind() {
                return k;
            }
            let notified = self.inner.kind_notify.notified();
            if let Some(k) = self.kind() {
                return k;
            }
            notified.await;
        }
    }

    /// Wait until **drain** begins — a graceful (`SIGINT`/`SIGTERM`) or
    /// immediate (`SIGQUIT`) trigger fired, or [`begin_drain`](Self::begin_drain)
    /// was called. `SIGHUP` (reload) does NOT trigger this, so a server's
    /// serve loop that awaits `wait_for_drain` keeps serving across reloads.
    ///
    /// Returns the kind that caused the drain.
    pub async fn wait_for_drain(&self) -> ShutdownKind {
        loop {
            if self.is_draining() {
                return self.kind().unwrap_or(ShutdownKind::Graceful);
            }
            let notified = self.inner.drain_notify.notified();
            if self.is_draining() {
                return self.kind().unwrap_or(ShutdownKind::Graceful);
            }
            notified.await;
        }
    }

    /// Programmatically begin draining (e.g. from a `Lifecycle.Drain` RPC, a
    /// custom [`crate::ExitSource`], or a manual call).
    ///
    /// A `Reload` kind does NOT set the drain bit (the server keeps serving).
    pub fn begin_drain(&self, kind: ShutdownKind) {
        self.inner.kind.store(kind.to_u8(), Ordering::Release);
        self.inner.kind_notify.notify_waiters();
        if kind.causes_drain() {
            self.inner.draining.store(true, Ordering::Release);
            self.inner.drain_notify.notify_waiters();
        }
    }

    /// Reset the drain bit (e.g. after handling a reload, to keep serving).
    pub fn clear_drain(&self) {
        self.inner.draining.store(false, Ordering::Release);
        self.inner.kind.store(ShutdownKind::NONE, Ordering::Release);
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn controller_starts_inactive() {
        let c = DrainController::new();
        assert_eq!(c.kind(), None);
        assert!(!c.is_draining());
    }

    #[test]
    fn begin_drain_sets_state() {
        let c = DrainController::new();
        c.begin_drain(ShutdownKind::Graceful);
        assert!(c.is_draining());
        assert_eq!(c.kind(), Some(ShutdownKind::Graceful));
    }

    #[test]
    fn reload_does_not_drain() {
        let c = DrainController::new();
        c.begin_drain(ShutdownKind::Reload);
        assert!(!c.is_draining());
        assert_eq!(c.kind(), Some(ShutdownKind::Reload));
        c.clear_drain();
        assert_eq!(c.kind(), None);
    }

    #[tokio::test]
    async fn wait_for_drain_unblocks_after_begin() {
        let c = DrainController::new();
        let c2 = c.clone();
        tokio::spawn(async move {
            tokio::time::sleep(std::time::Duration::from_millis(20)).await;
            c2.begin_drain(ShutdownKind::Immediate);
        });
        let k = c.wait_for_drain().await;
        assert_eq!(k, ShutdownKind::Immediate);
    }

    #[tokio::test]
    async fn wait_for_signal_returns_reload() {
        let c = DrainController::new();
        let c2 = c.clone();
        tokio::spawn(async move {
            tokio::time::sleep(std::time::Duration::from_millis(20)).await;
            c2.begin_drain(ShutdownKind::Reload);
        });
        let k = c.wait_for_signal().await;
        assert_eq!(k, ShutdownKind::Reload);
        assert!(!c.is_draining());
    }
}
