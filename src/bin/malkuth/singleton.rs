//! Singleton lock for the malkuth watchdog binary.
//!
//! Uses O_CREAT|O_EXCL (CREATE_NEW on Windows) to atomically create a
//! lock file keyed by proxy port. Reads the owning PID from the file
//! to determine if the old process is still alive.
//!
//! - Same binary mtime → refuses to start.
//! - Different mtime → SIGTERM old, wait, SIGKILL, proceed.
//! - Old process dead → removes stale lock file, proceeds.

use std::fs::{self, File, OpenOptions};
use std::io::{Read, Write};
use std::path::PathBuf;
use std::process;
use std::time::SystemTime;

pub struct SingletonGuard {
    _lock_path: PathBuf,
}

impl Drop for SingletonGuard {
    fn drop(&mut self) {
        let _ = fs::remove_file(&self._lock_path);
    }
}

#[derive(Debug)]
pub enum SingletonError {
    AlreadyRunning(u32),
    Io(std::io::Error),
}

impl std::fmt::Display for SingletonError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            SingletonError::AlreadyRunning(pid) => {
                write!(f, "another malkuth instance is already running (pid {pid})")
            }
            SingletonError::Io(e) => write!(f, "singleton lock I/O error: {e}"),
        }
    }
}

impl From<std::io::Error> for SingletonError {
    fn from(e: std::io::Error) -> Self { SingletonError::Io(e) }
}

// ── Platform: process liveness check ────────────────────────────

#[cfg(unix)]
fn is_process_alive(pid: u32) -> bool {
    if pid == 0 { return false; }
    unsafe { libc::kill(pid as i32, 0) == 0 }
}

#[cfg(unix)]
fn kill_process(pid: u32) -> std::io::Result<()> {
    let rc = unsafe { libc::kill(pid as i32, libc::SIGTERM) };
    if rc != 0 {
        let err = std::io::Error::last_os_error();
        if err.raw_os_error() == Some(libc::ESRCH) { return Ok(()); }
        return Err(err);
    }
    for _ in 0..20 {
        if !is_process_alive(pid) { return Ok(()); }
        std::thread::sleep(std::time::Duration::from_millis(100));
    }
    unsafe { libc::kill(pid as i32, libc::SIGKILL); }
    Ok(())
}

#[cfg(windows)]
fn is_process_alive(pid: u32) -> bool {
    if pid == 0 { return false; }
    std::process::Command::new("tasklist")
        .args(["/FI", &format!("PID eq {pid}"), "/NH"])
        .output()
        .map(|o| String::from_utf8_lossy(&o.stdout).contains(&pid.to_string()))
        .unwrap_or(false)
}

#[cfg(windows)]
fn kill_process(pid: u32) -> std::io::Result<()> {
    if !is_process_alive(pid) { return Ok(()); }
    let status = std::process::Command::new("taskkill")
        .args(["/F", "/PID", &pid.to_string()])
        .status()?;
    if !status.success() {
        return Err(std::io::Error::new(std::io::ErrorKind::Other, "taskkill failed"));
    }
    for _ in 0..20 {
        if !is_process_alive(pid) { return Ok(()); }
        std::thread::sleep(std::time::Duration::from_millis(100));
    }
    Ok(())
}

// ── Main acquire logic ──────────────────────────────────────────

pub fn acquire(proxy_port: u16) -> Result<SingletonGuard, SingletonError> {
    let lock_dir = lock_dir_path();
    fs::create_dir_all(&lock_dir).ok();
    let lock_path = lock_dir.join(format!("malkuth-{proxy_port}.lock"));
    let binary_mtime = get_binary_mtime();

    // O_CREAT|O_EXCL on Unix → CREATE_NEW on Windows.
    match OpenOptions::new().write(true).create_new(true).open(&lock_path) {
        Ok(mut file) => {
            let pid = process::id();
            let mut content = format!("{pid}\n");
            if let Some(mt) = &binary_mtime {
                if let Ok(dur) = mt.duration_since(SystemTime::UNIX_EPOCH) {
                    content.push_str(&format!("{}\n{}", dur.as_secs(), dur.subsec_nanos()));
                }
            }
            file.write_all(content.as_bytes())?;
            Ok(SingletonGuard { _lock_path: lock_path })
        }
        Err(e) if e.kind() == std::io::ErrorKind::AlreadyExists => {
            let (old_pid, old_mtime) = read_lock_info(&lock_path);

            if !is_process_alive(old_pid) {
                let _ = fs::remove_file(&lock_path);
                return acquire(proxy_port);
            }

            match (&binary_mtime, &old_mtime) {
                (Some(new_mt), Some(old_mt)) if new_mt == old_mt => {
                    Err(SingletonError::AlreadyRunning(old_pid))
                }
                (Some(_), Some(_)) => {
                    eprintln!("malkuth: killing old instance (pid {old_pid}, different build)");
                    let _ = kill_process(old_pid);
                    std::thread::sleep(std::time::Duration::from_millis(800));
                    let _ = fs::remove_file(&lock_path);
                    acquire(proxy_port)
                }
                _ => Err(SingletonError::AlreadyRunning(old_pid)),
            }
        }
        Err(e) => Err(SingletonError::Io(e)),
    }
}

fn lock_dir_path() -> PathBuf {
    #[cfg(target_os = "linux")]
    if fs::metadata("/run").is_ok() {
        return PathBuf::from("/run/malkuth");
    }
    #[cfg(unix)]
    {
        if let Ok(home) = std::env::var("HOME") {
            return PathBuf::from(home).join(".malkuth");
        }
    }
    std::env::temp_dir()
}

fn read_lock_info(lock_path: &PathBuf) -> (u32, Option<SystemTime>) {
    let mut buf = String::new();
    if let Ok(mut f) = File::open(lock_path) {
        let _ = f.read_to_string(&mut buf);
    }
    let lines: Vec<&str> = buf.trim().lines().collect();
    let pid: u32 = lines.first().and_then(|s| s.parse().ok()).unwrap_or(0);
    let mtime = if lines.len() >= 3 {
        let secs: u64 = lines[1].parse().unwrap_or(0);
        let nsecs: u32 = lines[2].parse().unwrap_or(0);
        SystemTime::UNIX_EPOCH.checked_add(std::time::Duration::new(secs, nsecs))
    } else {
        None
    };
    (pid, mtime)
}

fn get_binary_mtime() -> Option<SystemTime> {
    std::env::current_exe().ok()?.metadata().ok()?.modified().ok()
}
