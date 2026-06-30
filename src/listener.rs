//! Layer 3 — zero-downtime listener handoff.
//!
//! [`acquire_listener`] prefers a listener passed by systemd (socket
//! activation) and falls back to a plain bind. Under socket activation the
//! listening fd is held by systemd, so restarting the process does not drop
//! in-flight connections — the foundation of zero-downtime rolling updates.
//!
//! The systemd protocol is implemented in pure Rust (no `libsystemd`):
//! systemd sets `LISTEN_PID` (must equal our pid) and `LISTEN_FDS` (count),
//! with the first passed fd at number 3 (`SD_LISTEN_FDS_START`).

use std::io;

use tokio::net::TcpListener;
use tracing::info;

/// Acquire a TCP listener, preferring systemd socket activation when the
/// `socket-activation` feature is enabled and the environment indicates an
/// inherited fd; otherwise bind `addr` afresh.
pub async fn acquire_listener(addr: &str) -> io::Result<TcpListener> {
    #[cfg(feature = "socket-activation")]
    {
        if let Some(listener) = from_systemd()? {
            info!(
                event = "listener_from_socket_activation",
                "acquired listener from systemd (fd inherited)"
            );
            return Ok(listener);
        }
    }
    info!(
        event = "listener_bind",
        addr = addr,
        "binding listener afresh"
    );
    TcpListener::bind(addr).await
}

// ── systemd fd inheritance (unix + socket-activation) ──────────

/// The first fd number systemd hands over.
#[cfg(all(unix, feature = "socket-activation"))]
const SD_LISTEN_FDS_START: i32 = 3;

#[cfg(all(unix, feature = "socket-activation"))]
fn from_systemd() -> io::Result<Option<TcpListener>> {
    use std::os::fd::FromRawFd;
    use tracing::warn;

    let pid: u32 = match std::env::var("LISTEN_PID")
        .ok()
        .and_then(|v| v.parse().ok())
    {
        Some(p) => p,
        None => return Ok(None),
    };
    if pid != std::process::id() {
        return Ok(None);
    }
    let n: usize = match std::env::var("LISTEN_FDS")
        .ok()
        .and_then(|v| v.parse().ok())
    {
        Some(n) => n,
        None => return Ok(None),
    };
    if n == 0 {
        return Ok(None);
    }

    // Take ownership of fd 3. systemd passes it as a valid, open socket when
    // LISTEN_PID/LISTEN_FDS are set as validated above.
    let std_listener = unsafe { std::net::TcpListener::from_raw_fd(SD_LISTEN_FDS_START) };
    if let Err(e) = std_listener.set_nonblocking(true) {
        warn!(error = %e, "failed to set non-blocking on inherited fd");
    }
    match TcpListener::from_std(std_listener) {
        Ok(l) => Ok(Some(l)),
        Err(e) => {
            warn!(error = %e, "inherited fd was not a TCP socket");
            Ok(None)
        }
    }
}

// On non-unix with the feature enabled there is nothing to inherit.
#[cfg(all(not(unix), feature = "socket-activation"))]
#[allow(clippy::unnecessary_wraps)]
fn from_systemd() -> io::Result<Option<TcpListener>> {
    Ok(None)
}
