//! Singleton lock for the malkuth watchdog binary.
//!
//! Uses O_CREAT|O_EXCL (CREATE_NEW on Windows) to atomically create a
//! lock file keyed by proxy port. The lock file records:
//! - PID
//! - Creation time (Unix epoch seconds.nanos)
//! - Binary build mtime (Unix epoch seconds.nanos)
//! - Binary path
//! - Working directory
//!
//! Lock directory defaults to /tmp/malkuth-locks (Unix) / %TEMP%\malkuth-locks
//! (Windows). Override via MALKUTH_LOCK_DIR environment variable.
//!
//! - Same binary mtime → refuses to start.
//! - Different mtime → SIGTERM old, wait, SIGKILL, proceed.
//! - Old process dead → removes stale lock file, proceeds.

use std::fs::{self, File, OpenOptions};
use std::io::{Read, Write};
use std::path::PathBuf;
use std::process;
use std::time::SystemTime;

/// Metadata written into every lock file.
struct LockMeta {
    pid: u32,
    created_at: SystemTime,
    build_mtime: SystemTime,
    binary_path: PathBuf,
    working_dir: PathBuf,
}

impl LockMeta {
    fn serialize(&self) -> String {
        let c = self.created_at.duration_since(SystemTime::UNIX_EPOCH).unwrap_or_default();
        let b = self.build_mtime.duration_since(SystemTime::UNIX_EPOCH).unwrap_or_default();
        format!(
            "{}\n{}.{}\n{}.{}\n{}\n{}",
            self.pid,
            c.as_secs(), c.subsec_nanos(),
            b.as_secs(), b.subsec_nanos(),
            self.binary_path.display(),
            self.working_dir.display(),
        )
    }

    fn deserialize(raw: &str) -> Option<Self> {
        let lines: Vec<&str> = raw.trim().lines().collect();
        if lines.len() < 5 { return None; }
        let pid: u32 = lines[0].parse().ok()?;
        let created_at = parse_time(lines[1])?;
        let build_mtime = parse_time(lines[2])?;
        let binary_path = PathBuf::from(lines[3]);
        let working_dir = PathBuf::from(lines[4]);
        Some(LockMeta { pid, created_at, build_mtime, binary_path, working_dir })
    }
}

fn parse_time(s: &str) -> Option<SystemTime> {
    let (secs, nsecs) = s.split_once('.')?;
    let s: u64 = secs.parse().ok()?;
    let n: u32 = nsecs.parse().ok()?;
    SystemTime::UNIX_EPOCH.checked_add(std::time::Duration::new(s, n))
}

fn format_time(t: SystemTime) -> String {
    let d = t.duration_since(SystemTime::UNIX_EPOCH).unwrap_or_default();
    format!("{}.{}", d.as_secs(), d.subsec_nanos())
}

pub struct SingletonGuard {
    _lock_path: PathBuf,
}

impl Drop for SingletonGuard {
    fn drop(&mut self) { let _ = fs::remove_file(&self._lock_path); }
}

#[derive(Debug)]
pub enum SingletonError {
    AlreadyRunning(u32),
    Io(std::io::Error),
}

impl std::fmt::Display for SingletonError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            SingletonError::AlreadyRunning(pid) => write!(f, "another malkuth instance is already running (pid {pid})"),
            SingletonError::Io(e) => write!(f, "singleton lock I/O error: {e}"),
        }
    }
}

impl From<std::io::Error> for SingletonError {
    fn from(e: std::io::Error) -> Self { SingletonError::Io(e) }
}

// ── Platform helpers ────────────────────────────────────────────

#[cfg(unix)]
fn is_process_alive(pid: u32) -> bool {
    if pid == 0 { return false; }
    unsafe { libc::kill(pid as i32, 0) == 0 }
}

#[cfg(unix)]
fn kill_process(pid: u32) -> std::io::Result<()> {
    let rc = unsafe { libc::kill(pid as i32, libc::SIGTERM) };
    if rc != 0 {
        let e = std::io::Error::last_os_error();
        if e.raw_os_error() == Some(libc::ESRCH) { return Ok(()); }
        return Err(e);
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
    let s = std::process::Command::new("taskkill").args(["/F", "/PID", &pid.to_string()]).status()?;
    if !s.success() { return Err(std::io::Error::new(std::io::ErrorKind::Other, "taskkill failed")); }
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

    let meta = LockMeta {
        pid: process::id(),
        created_at: SystemTime::now(),
        build_mtime: get_binary_mtime().unwrap_or(SystemTime::UNIX_EPOCH),
        binary_path: std::env::current_exe().unwrap_or_default(),
        working_dir: std::env::current_dir().unwrap_or_default(),
    };

    match OpenOptions::new().write(true).create_new(true).open(&lock_path) {
        Ok(mut file) => {
            file.write_all(meta.serialize().as_bytes())?;
            Ok(SingletonGuard { _lock_path: lock_path })
        }
        Err(e) if e.kind() == std::io::ErrorKind::AlreadyExists => {
            let old_meta = read_meta(&lock_path);

            if !is_process_alive(old_meta.pid) {
                let _ = fs::remove_file(&lock_path);
                return acquire(proxy_port);
            }

            if meta.build_mtime == old_meta.build_mtime {
                return Err(SingletonError::AlreadyRunning(old_meta.pid));
            }

            eprintln!("malkuth: killing old instance (pid {}, different build)", old_meta.pid);
            let _ = kill_process(old_meta.pid);
            std::thread::sleep(std::time::Duration::from_millis(800));
            let _ = fs::remove_file(&lock_path);
            acquire(proxy_port)
        }
        Err(e) => Err(SingletonError::Io(e)),
    }
}

fn lock_dir_path() -> PathBuf {
    if let Ok(dir) = std::env::var("MALKUTH_LOCK_DIR") {
        return PathBuf::from(dir);
    }
    #[cfg(unix)]
    { PathBuf::from("/tmp/malkuth-locks") }
    #[cfg(windows)]
    { std::env::temp_dir().join("malkuth-locks") }
}

fn read_meta(lock_path: &PathBuf) -> LockMeta {
    let mut buf = String::new();
    if let Ok(mut f) = File::open(lock_path) {
        let _ = f.read_to_string(&mut buf);
    }
    LockMeta::deserialize(&buf).unwrap_or(LockMeta {
        pid: 0,
        created_at: SystemTime::UNIX_EPOCH,
        build_mtime: SystemTime::UNIX_EPOCH,
        binary_path: PathBuf::new(),
        working_dir: PathBuf::new(),
    })
}

fn get_binary_mtime() -> Option<SystemTime> {
    std::env::current_exe().ok()?.metadata().ok()?.modified().ok()
}
