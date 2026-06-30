//! Default [`ExitSource`] driven by OS signals.
//!
//! Canonical convention:
//! - `SIGINT` / `SIGTERM` → graceful drain (exit)
//! - `SIGQUIT`           → immediate exit
//! - `SIGHUP`            → hot reload (do **not** exit; keep serving)
//!
//! Swap in your own `ExitSource` if you want drain triggered by something else
//! (e.g. an in-band "stop" RPC, or a parent supervisor signal over IPC).
//! Built on `async-signal` → runtime-agnostic.

use async_trait::async_trait;
use futures_util::StreamExt;
use malkuth_core::{DrainController, ExitReason, ExitSource, ShutdownKind};
use tracing::{info, warn};

/// OS-signal-driven exit source.
pub struct SignalExitSource;

#[cfg(unix)]
#[async_trait]
impl ExitSource for SignalExitSource {
    async fn wait(&self, ctrl: DrainController) -> ExitReason {
        use async_signal::Signal;
        let signals = match async_signal::Signals::new([
            Signal::Int,
            Signal::Term,
            Signal::Hup,
            Signal::Quit,
        ]) {
            Ok(s) => s,
            Err(e) => {
                warn!(error = %e, "failed to install signal handlers; exiting gracefully");
                return ExitReason::graceful();
            }
        };
        let mut stream = signals;
        while let Some(sig) = stream.next().await {
            let sig = match sig {
                Ok(s) => s,
                Err(e) => {
                    warn!(error = %e, "signal stream error");
                    continue;
                }
            };
            let (kind, should_exit) = match sig {
                Signal::Int | Signal::Term => {
                    info!(?sig, "signal → graceful drain");
                    (ShutdownKind::Graceful, true)
                }
                Signal::Quit => {
                    warn!(?sig, "signal → immediate exit");
                    (ShutdownKind::Immediate, true)
                }
                Signal::Hup => {
                    info!(?sig, "signal → reload (no exit)");
                    (ShutdownKind::Reload, false)
                }
                other => {
                    warn!(?other, "ignoring unhandled signal");
                    continue;
                }
            };
            ctrl.begin_drain(kind);
            if should_exit {
                return ExitReason { kind, should_exit };
            }
        }
        ExitReason::graceful()
    }
}

#[cfg(not(unix))]
#[async_trait]
impl ExitSource for SignalExitSource {
    async fn wait(&self, ctrl: DrainController) -> ExitReason {
        let signals = match async_signal::Signals::new([async_signal::Signal::Int]) {
            Ok(s) => s,
            Err(_) => return ExitReason::graceful(),
        };
        let mut stream = signals;
        while let Some(sig) = stream.next().await {
            if sig.is_ok() {
                info!("ctrl_c → graceful drain");
                ctrl.begin_drain(ShutdownKind::Graceful);
                return ExitReason::graceful();
            }
        }
        ExitReason::graceful()
    }
}
