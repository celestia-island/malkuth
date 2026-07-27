//! Self-update: spawn a new binary inheriting a listener fd.
//!
//! Uses `std::process::Command::pre_exec()` to clear `FD_CLOEXEC` on the
//! listener fd before the child execs, then passes the fd number via the
//! `MALKUTH_LISTEN_FD` environment variable.
//!
//! All other fds with `FD_CLOEXEC` set (tokio's eventfd, timer fd, epoll fd,
//! etc.) are automatically closed by the kernel at exec time — no blind
//! close loop, no undefined behaviour.

use std::os::unix::{io::RawFd, process::CommandExt};

/// Environment variable name used to hand the inherited fd number to the
/// child process.
pub const LISTEN_FD_ENV: &str = "MALKUTH_LISTEN_FD";

/// Spawn `program` with `args` as a child process, preserving `listen_fd`
/// across the exec boundary by clearing its `FD_CLOEXEC` flag in the child's
/// `pre_exec` hook.
///
/// On success the returned [`std::process::Child`] represents the new
/// process.  The caller is responsible for waiting on it and/or terminating
/// the current (parent) process once handoff is complete.
///
/// # Safety
///
/// `pre_exec` runs in the child after `fork(2)` but before `execve(2)`.
/// Only async-signal-safe operations are legal there; `fcntl` is safe.
pub fn spawn_with_listen_fd(
    program: &str,
    args: &[String],
    listen_fd: RawFd,
) -> std::io::Result<std::process::Child> {
    let mut cmd = std::process::Command::new(program);
    cmd.args(args);
    cmd.env(LISTEN_FD_ENV, listen_fd.to_string());

    unsafe {
        cmd.pre_exec(move || {
            let flags = libc::fcntl(listen_fd, libc::F_GETFD);
            if flags != -1 {
                libc::fcntl(listen_fd, libc::F_SETFD, flags & !libc::FD_CLOEXEC);
            }
            Ok(())
        });
    }

    cmd.spawn()
}

/// Convenience wrapper that spawns the current binary (as returned by
/// [`std::env::current_exe`]) with the original command-line arguments,
/// passing `listen_fd` to the child.
pub fn spawn_self(listen_fd: RawFd) -> std::io::Result<std::process::Child> {
    let exe = std::env::current_exe()?;
    let args: Vec<String> = std::env::args().skip(1).collect();
    spawn_with_listen_fd(&exe.to_string_lossy(), &args, listen_fd)
}
