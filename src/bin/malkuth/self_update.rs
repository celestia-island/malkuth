//! Self-update — fork+exec based zero-downtime restart for the malkuth daemon.
//!
//! When a new malkuth binary is available, the running daemon can:
//! 1. Fork a child process
//! 2. Child exec's the new binary with the listener fd inherited
//! 3. Parent gracefully drains and exits
//! 4. New process takes over the listener immediately
//!
//! This is the final piece (Phase 6) of the malkuth supervision lifecycle.
//!
//! ## Usage
//!
//! ```ignore
//! # Old daemon detects new binary and triggers self-update:
//! malkuth daemon --config malkuth.toml --self-update /path/to/new/malkuth
//!
//! # New process starts with inherited fd:
//! malkuth --takeover LISTEN_FD=5 daemon --config malkuth.toml
//! ```

use std::os::unix::process::CommandExt;
use std::process::Command;

use tracing::{error, info};

/// Environment variable used to pass the inherited listener fd.
const LISTEN_FD_ENV: &str = "MALKUTH_LISTEN_FD";

/// Fork the current process, exec the new binary with the listener fd inherited,
/// and exit the parent after draining.
///
/// Returns `true` in the child (new process), `false` in the parent (old process).
/// The caller should:
/// - In the parent: begin drain, wait for workers, exit.
/// - In the child: take over the listener and start serving.
pub fn fork_and_exec(
    new_binary: &str,
    listener_fd: Option<i32>,
    extra_args: &[String],
) -> std::io::Result<bool> {
    let pid = unsafe { libc::fork() };
    if pid < 0 {
        return Err(std::io::Error::last_os_error());
    }
    if pid > 0 {
        // Parent — old process.
        info!(child_pid = pid, "forked new malkuth process; beginning drain");
        return Ok(false);
    }

    // Child — new process.
    // Close all fds except stdin/stdout/stderr and the listener.
    if let Some(fd) = listener_fd {
        let max_fd = unsafe { libc::sysconf(libc::_SC_OPEN_MAX) };
        let max = if max_fd > 0 { max_fd as i32 } else { 1024 };
        for f in 3..max {
            if f != fd {
                unsafe { libc::close(f) };
            }
        }
        unsafe {
            libc::setenv(
                LISTEN_FD_ENV.as_ptr() as *const libc::c_char,
                format!("{fd}").as_ptr() as *const libc::c_char,
                1,
            );
        }
    }

    let mut cmd = Command::new(new_binary);
    cmd.args(extra_args);
    // Pre-exec: inherit all fds from fork. The only relevant one is the listener.
    unsafe {
        cmd.pre_exec(|| Ok(()));
    }

    let err = cmd.exec();
    // Only reaches here if exec fails.
    error!(error = %err, binary = new_binary, "exec of new malkuth binary failed");
    std::process::exit(1);
}

/// Check if the current process was started with an inherited listener fd.
/// Returns the fd number if `LISTEN_FD_ENV` is set and valid.
pub fn inherited_listener_fd() -> Option<i32> {
    let val: i32 = std::env::var(LISTEN_FD_ENV).ok()?.parse().ok()?;
    if val >= 3 {
        Some(val)
    } else {
        None
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn inherited_fd_none_when_env_unset() {
        unsafe { std::env::remove_var(LISTEN_FD_ENV) };
        assert!(inherited_listener_fd().is_none());
    }

    #[test]
    fn inherited_fd_parses_valid() {
        unsafe { std::env::set_var(LISTEN_FD_ENV, "5") };
        assert_eq!(inherited_listener_fd(), Some(5));
        unsafe { std::env::remove_var(LISTEN_FD_ENV) };
    }

    #[test]
    fn inherited_fd_ignores_invalid() {
        unsafe { std::env::set_var(LISTEN_FD_ENV, "not_a_number") };
        assert!(inherited_listener_fd().is_none());
        unsafe { std::env::remove_var(LISTEN_FD_ENV) };
    }
}
