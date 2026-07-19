//! Singleton lock for the malkuth watchdog binary.
//!
//! On start, acquires an exclusive file lock keyed by the proxy port.
//! If another malkuth instance holds the lock:
//! - Same binary (mtime match) → refuses to start.
//! - Different binary (mtime differs) → kills the old instance and proceeds.
//! - Old process dead → stale lock, overwrites and proceeds.
//!
//! The lock is released when the process exits (via Drop).

use std::fs::{self, File, OpenOptions};
use std::io::{Read, Write};
use std::os::unix::io::AsRawFd;
use std::path::PathBuf;
use std::process;
use std::time::SystemTime;

pub struct SingletonGuard {
    _file: File,
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

pub fn acquire(proxy_port: u16) -> Result<SingletonGuard, SingletonError> {
    let lock_dir = if cfg!(target_os = "linux") && fs::metadata("/run").is_ok() {
        let d = PathBuf::from("/run/malkuth");
        fs::create_dir_all(&d).ok();
        d
    } else {
        std::env::temp_dir()
    };
    let lock_path = lock_dir.join(format!("malkuth-{proxy_port}.lock"));
    let binary_mtime = get_binary_mtime();

    // try_lock returns Some(file) on success, None on contention
    match try_lock(&lock_path) {
        Ok(file) => {
            write_lock_info(&lock_path, &binary_mtime)?;
            return Ok(SingletonGuard { _file: file, _lock_path: lock_path });
        }
        Err(_contended) => {
            // Lock held — read who holds it
            let (old_pid, old_mtime) = read_lock_info(&lock_path);

            if !is_process_alive(old_pid) {
                // Stale lock — remove and retry
                let _ = fs::remove_file(&lock_path);
                return acquire(proxy_port);
            }

            match (&binary_mtime, &old_mtime) {
                (Some(new_mt), Some(old_mt)) if new_mt == old_mt => {
                    return Err(SingletonError::AlreadyRunning(old_pid));
                }
                _ => {
                    eprintln!("malkuth: killing old instance (pid {old_pid}, different build)");
                    let _ = kill_process(old_pid);
                    // Brief wait for cleanup
                    std::thread::sleep(std::time::Duration::from_millis(800));
                    // Remove stale lock file and retry
                    let _ = fs::remove_file(&lock_path);
                    return acquire(proxy_port);
                }
            }
        }
    }
}

fn try_lock(path: &PathBuf) -> Result<File, ()> {
    let file = OpenOptions::new()
        .read(true)
        .write(true)
        .create(true)
        .truncate(false)
        .open(path)
        .map_err(|_| ())?;
    let rc = unsafe { libc::flock(file.as_raw_fd(), libc::LOCK_EX | libc::LOCK_NB) };
    if rc != 0 {
        let err = std::io::Error::last_os_error();
        if err.kind() == std::io::ErrorKind::WouldBlock || err.raw_os_error() == Some(libc::EAGAIN) || err.raw_os_error() == Some(libc::EWOULDBLOCK) {
            drop(file);
            return Err(());
        }
        drop(file);
        return Err(());
    }
    Ok(file)
}

fn write_lock_info(lock_path: &PathBuf, mtime: &Option<SystemTime>) -> std::io::Result<()> {
    let pid = process::id();
    let mut content = format!("{pid}\n");
    if let Some(mt) = mtime {
        if let Ok(dur) = mt.duration_since(SystemTime::UNIX_EPOCH) {
            content.push_str(&format!("{}\n{}", dur.as_secs(), dur.subsec_nanos()));
        }
    }
    let mut f = OpenOptions::new().write(true).truncate(true).open(lock_path)?;
    f.write_all(content.as_bytes())
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

fn is_process_alive(pid: u32) -> bool {
    if pid == 0 { return false; }
    unsafe { libc::kill(pid as i32, 0) == 0 }
}

fn kill_process(pid: u32) -> std::io::Result<()> {
    // SIGTERM
    let rc = unsafe { libc::kill(pid as i32, libc::SIGTERM) };
    if rc != 0 {
        let err = std::io::Error::last_os_error();
        if err.raw_os_error() == Some(libc::ESRCH) { return Ok(()); }
        return Err(err);
    }
    // Wait up to 2s for graceful exit
    for _ in 0..20 {
        if !is_process_alive(pid) { return Ok(()); }
        std::thread::sleep(std::time::Duration::from_millis(100));
    }
    // SIGKILL
    unsafe { libc::kill(pid as i32, libc::SIGKILL); }
    Ok(())
}
