//! Default [`ExitSource`] driven by OS signals (tokio).
//!
//! Canonical convention:
//! - `SIGINT` / `SIGTERM` → graceful drain (exit)
//! - `SIGQUIT`           → immediate exit
//! - `SIGHUP`            → hot reload (do **not** exit; keep serving)
//!
//! Swap in your own `ExitSource` if you want drain triggered by something else
//! (e.g. an in-band "stop" RPC, or a parent supervisor signal over IPC).

use crate::{DrainController, ExitReason, ExitSource, ShutdownKind};
use async_trait::async_trait;
use tracing::{info, warn};

/// OS-signal-driven exit source.
pub struct SignalExitSource;

#[cfg(unix)]
#[async_trait]
impl ExitSource for SignalExitSource {
    async fn wait(&self, ctrl: DrainController) -> ExitReason {
        use tokio::signal::unix::{SignalKind, signal};

        let mut sig_int = match signal(SignalKind::interrupt()) {
            Ok(s) => s,
            Err(e) => {
                warn!(error = %e, "failed to install SIGINT handler");
                return ExitReason::graceful();
            }
        };
        let mut sig_term = match signal(SignalKind::terminate()) {
            Ok(s) => s,
            Err(e) => {
                warn!(error = %e, "failed to install SIGTERM handler");
                return ExitReason::graceful();
            }
        };
        let mut sig_hup = match signal(SignalKind::hangup()) {
            Ok(s) => s,
            Err(e) => {
                warn!(error = %e, "failed to install SIGHUP handler");
                return ExitReason::graceful();
            }
        };
        let mut sig_quit = match signal(SignalKind::quit()) {
            Ok(s) => s,
            Err(e) => {
                warn!(error = %e, "failed to install SIGQUIT handler");
                return ExitReason::graceful();
            }
        };

        loop {
            tokio::select! {
                _ = sig_int.recv() => {
                    info!("SIGINT → graceful drain");
                    ctrl.begin_drain(ShutdownKind::Graceful);
                    return ExitReason::graceful();
                }
                _ = sig_term.recv() => {
                    info!("SIGTERM → graceful drain");
                    ctrl.begin_drain(ShutdownKind::Graceful);
                    return ExitReason::graceful();
                }
                _ = sig_quit.recv() => {
                    warn!("SIGQUIT → immediate exit");
                    ctrl.begin_drain(ShutdownKind::Immediate);
                    return ExitReason::immediate();
                }
                _ = sig_hup.recv() => {
                    info!("SIGHUP → reload (no exit)");
                    ctrl.begin_drain(ShutdownKind::Reload);
                }
            }
        }
    }
}

#[cfg(not(unix))]
#[async_trait]
impl ExitSource for SignalExitSource {
    async fn wait(&self, ctrl: DrainController) -> ExitReason {
        if tokio::signal::ctrl_c().await.is_ok() {
            info!("ctrl_c → graceful drain");
            ctrl.begin_drain(ShutdownKind::Graceful);
            return ExitReason::graceful();
        }
        ExitReason::graceful()
    }
}
