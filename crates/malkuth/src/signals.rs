//! Default [`ExitSource`] driven by OS signals.
//!
//! Implements the canonical convention:
//! - `SIGINT` / `SIGTERM` → graceful drain (exit)
//! - `SIGQUIT`            → immediate exit
//! - `SIGHUP`             → hot reload (do **not** exit; keep serving)
//!
//! Swap in your own `ExitSource` if you want drain triggered by something else
//! (e.g. an in-band "stop" RPC your server receives, or a parent supervisor
//! signal over IPC). Built on `async-signal` → runtime-agnostic.

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
            Signal::Interrupt,
            Signal::Terminate,
            Signal::Hangup,
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
            let (kind, should_exit) = match sig {
                Signal::Interrupt | Signal::Terminate => {
                    info!(?sig, "signal → graceful drain");
                    (ShutdownKind::Graceful, true)
                }
                Signal::Quit => {
                    warn!(?sig, "signal → immediate exit");
                    (ShutdownKind::Immediate, true)
                }
                Signal::Hangup => {
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
            // reload: keep listening for further signals.
        }
        ExitReason::graceful()
    }
}

#[cfg(not(unix))]
#[async_trait]
impl ExitSource for SignalExitSource {
    async fn wait(&self, ctrl: DrainController) -> ExitReason {
        // Non-unix fallback: only ctrl_c maps to graceful drain.
        let signals = async_signal::Signals::new([async_signal::Signal::Interrupt])
            .unwrap_or_else(|_| panic!("install signals"));
        let mut stream = signals;
        if let Some(_sig) = stream.next().await {
            info!("ctrl_c → graceful drain");
            ctrl.begin_drain(ShutdownKind::Graceful);
        }
        ExitReason::graceful()
    }
}
