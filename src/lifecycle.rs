//! Layer 1 — uniform signal semantics & drain controller.
//!
//! Canonical convention (nginx/Go):
//! - `SIGINT` / `SIGTERM` → graceful drain
//! - `SIGHUP`            → hot config reload (no exit)
//! - `SIGQUIT`           → immediate exit (emergency only)
//!
//! The single biggest shared gap across all three projects today is that
//! their servers only catch `SIGINT` (`ctrl_c`), so `docker stop` /
//! `systemctl restart` — which send `SIGTERM` — bypass graceful shutdown.
//! [`DrainController::install`] fixes that in one place.

use std::time::Duration;

use tokio::sync::watch;
use tracing::{info, warn};

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

/// Controller installed by [`DrainController::install`].
///
/// Cloning is cheap (it only holds `tokio::watch` senders). Pass clones to
/// the drain phase, the probe layer, and the main serve loop.
#[derive(Clone)]
pub struct DrainController {
    kind_tx: watch::Sender<Option<ShutdownKind>>,
    draining_tx: watch::Sender<bool>,
}

impl DrainController {
    /// Install the canonical signal handlers and return a controller.
    ///
    /// On Unix this traps `SIGINT` / `SIGTERM` / `SIGHUP` / `SIGQUIT`; on
    /// other platforms it falls back to `ctrl_c` → graceful.
    #[must_use]
    pub fn install() -> Self {
        let (kind_tx, _) = watch::channel(None);
        let (draining_tx, _) = watch::channel(false);
        spawn_signal_loop(kind_tx.clone(), draining_tx.clone());
        Self {
            kind_tx,
            draining_tx,
        }
    }

    /// Current shutdown kind, if any signal has fired.
    pub fn kind(&self) -> Option<ShutdownKind> {
        *self.kind_tx.borrow()
    }

    /// Whether drain has begun (a graceful/immediate signal fired, or
    /// [`begin_drain`](Self::begin_drain) was called).
    pub fn is_draining(&self) -> bool {
        *self.draining_tx.borrow()
    }

    /// Wait until a shutdown/reload signal fires and return its kind.
    ///
    /// Returns immediately if one has already fired.
    pub async fn wait_for_signal(&self) -> ShutdownKind {
        let mut rx = self.kind_tx.subscribe();
        loop {
            if let Some(k) = *rx.borrow() {
                return k;
            }
            if rx.changed().await.is_err() {
                return ShutdownKind::Graceful;
            }
        }
    }

    /// Wait until **drain** begins — a graceful (`SIGINT`/`SIGTERM`) or
    /// immediate (`SIGQUIT`) signal fired, or [`begin_drain`](Self::begin_drain)
    /// was called. `SIGHUP` (reload) does NOT trigger this, so a server's
    /// serve loop that awaits `wait_for_drain` keeps serving across reloads.
    ///
    /// Returns the kind that caused the drain.
    pub async fn wait_for_drain(&self) -> ShutdownKind {
        let mut drx = self.draining_tx.subscribe();
        if *drx.borrow() {
            return self.kind().unwrap_or(ShutdownKind::Graceful);
        }
        let _ = drx.changed().await;
        self.kind().unwrap_or(ShutdownKind::Graceful)
    }

    /// Programmatically begin draining (e.g. from a `Lifecycle.Drain` RPC).
    pub fn begin_drain(&self, kind: ShutdownKind) {
        let _ = self.draining_tx.send(true);
        let _ = self.kind_tx.send(Some(kind));
    }

    /// Sleep for at most `timeout`, but wake as soon as drain begins.
    pub async fn sleep_or_drain(&self, timeout: Duration) {
        let mut rx = self.draining_tx.subscribe();
        if *rx.borrow() {
            return;
        }
        let _ = tokio::time::timeout(timeout, rx.changed()).await;
    }
}

impl Default for DrainController {
    fn default() -> Self {
        Self::install()
    }
}

fn trigger(
    kind_tx: &watch::Sender<Option<ShutdownKind>>,
    draining_tx: &watch::Sender<bool>,
    kind: ShutdownKind,
) {
    if matches!(kind, ShutdownKind::Graceful | ShutdownKind::Immediate) {
        let _ = draining_tx.send(true);
    }
    let _ = kind_tx.send(Some(kind));
}

#[cfg(unix)]
fn spawn_signal_loop(
    kind_tx: watch::Sender<Option<ShutdownKind>>,
    draining_tx: watch::Sender<bool>,
) {
    use tokio::signal::unix::{SignalKind, signal};

    tokio::spawn(async move {
        let mut sigint = match signal(SignalKind::interrupt()) {
            Ok(s) => s,
            Err(e) => {
                warn!(error = %e, "failed to install SIGINT handler");
                return;
            }
        };
        let mut sigterm = match signal(SignalKind::terminate()) {
            Ok(s) => s,
            Err(e) => {
                warn!(error = %e, "failed to install SIGTERM handler");
                return;
            }
        };
        let mut sighup = match signal(SignalKind::hangup()) {
            Ok(s) => s,
            Err(e) => {
                warn!(error = %e, "failed to install SIGHUP handler");
                return;
            }
        };
        let mut sigquit = match signal(SignalKind::quit()) {
            Ok(s) => s,
            Err(e) => {
                warn!(error = %e, "failed to install SIGQUIT handler");
                return;
            }
        };

        loop {
            tokio::select! {
                _ = sigint.recv() => {
                    info!("SIGINT received → graceful drain");
                    trigger(&kind_tx, &draining_tx, ShutdownKind::Graceful);
                    return;
                }
                _ = sigterm.recv() => {
                    info!("SIGTERM received → graceful drain");
                    trigger(&kind_tx, &draining_tx, ShutdownKind::Graceful);
                    return;
                }
                _ = sigquit.recv() => {
                    warn!("SIGQUIT received → immediate exit");
                    trigger(&kind_tx, &draining_tx, ShutdownKind::Immediate);
                    return;
                }
                _ = sighup.recv() => {
                    info!("SIGHUP received → reload");
                    // Reload does not drain and does not exit the loop:
                    // the caller performs a reload and keeps serving.
                    let _ = kind_tx.send(Some(ShutdownKind::Reload));
                }
            }
        }
    });
}

#[cfg(not(unix))]
fn spawn_signal_loop(
    kind_tx: watch::Sender<Option<ShutdownKind>>,
    draining_tx: watch::Sender<bool>,
) {
    tokio::spawn(async move {
        if tokio::signal::ctrl_c().await.is_ok() {
            info!("ctrl_c received → graceful drain");
            trigger(&kind_tx, &draining_tx, ShutdownKind::Graceful);
        }
    });
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn controller_starts_inactive() {
        // Just exercise construction without installing duplicate handlers.
        let (kind_tx, _) = watch::channel(None);
        let (draining_tx, _) = watch::channel(false);
        let ctrl = DrainController {
            kind_tx,
            draining_tx,
        };
        assert_eq!(ctrl.kind(), None);
        assert!(!ctrl.is_draining());
    }
}
