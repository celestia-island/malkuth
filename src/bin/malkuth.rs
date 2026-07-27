//! `malkuth` — watchdog-style supervisor binary.
//!
//! Wraps any program (even one that does not use the malkuth library) with a
//! supervised pod pool, optional file watching, and an L4 sticky reverse proxy.

#[path = "malkuth/cli.rs"]
mod cli;
#[path = "malkuth/ipc_proxy.rs"]
#[cfg(feature = "ipc")]
mod ipc_proxy;
#[path = "malkuth/pool.rs"]
mod pool;
#[path = "malkuth/proxy.rs"]
mod proxy;
#[path = "malkuth/self_update.rs"]
#[cfg(unix)]
mod self_update;
#[path = "malkuth/singleton.rs"]
mod singleton;
#[path = "malkuth/watcher.rs"]
mod watcher;
#[path = "malkuth/ws_proxy.rs"]
#[cfg(feature = "ws")]
mod ws_proxy;

use std::{net::SocketAddr, sync::Arc, time::Duration};
use tokio::signal;

use clap::Parser;
use cli::{Args, ProxySpec};
#[cfg(feature = "ipc")]
use ipc_proxy::run_ipc_proxy;
use pool::{PodManager, assign_ports};
use proxy::ProxyState;
use std::collections::HashMap;
use std::path::PathBuf;
use std::process::Stdio;
use tracing::{error, info, warn};
#[cfg(feature = "ws")]
#[cfg(feature = "ws")]
use ws_proxy::run_ws_proxy;

const DEFAULT_VERSION: &str = env!("CARGO_PKG_VERSION");

/// Recursively snapshot file mtimes from all watched paths.
/// Returns a map of path → last-modified time.
fn snapshot_mtimes(paths: &[PathBuf]) -> HashMap<PathBuf, std::time::SystemTime> {
    let mut map = HashMap::new();
    for root in paths {
        if !root.exists() {
            continue;
        }
        let mut stack = vec![root.clone()];
        while let Some(dir) = stack.pop() {
            if let Ok(entries) = std::fs::read_dir(&dir) {
                for entry in entries.flatten() {
                    let path = entry.path();
                    if path.is_dir() {
                        stack.push(path);
                    } else if let Ok(meta) = entry.metadata() {
                        if let Ok(mtime) = meta.modified() {
                            map.insert(path, mtime);
                        }
                    }
                }
            }
        }
    }
    map
}

/// Compare two mtime snapshots — true if any file was added, removed, or modified.
fn mtimes_changed(
    before: &HashMap<PathBuf, std::time::SystemTime>,
    after: &HashMap<PathBuf, std::time::SystemTime>,
) -> bool {
    if before.len() != after.len() {
        return true;
    }
    for (path, mtime) in before {
        match after.get(path) {
            Some(t) if t == mtime => continue,
            _ => return true,
        }
    }
    false
}

/// Collect metadata for a supervised binary: compile timestamp and SHA-256 hash.
fn collect_binary_info(program: &str) -> Option<malkuth::info_page::BinaryInfo> {
    let path = std::path::PathBuf::from(program);
    if !path.exists() {
        return None;
    }
    let name = path
        .file_name()
        .map(|n| n.to_string_lossy().into_owned())
        .unwrap_or_else(|| program.to_string());
    let path_str = path.display().to_string();

    let compile_time = path
        .metadata()
        .ok()
        .and_then(|m| m.modified().ok())
        .map(|t| {
            let dt: chrono::DateTime<chrono::Local> = t.into();
            dt.format("%Y-%m-%d %H:%M:%S").to_string()
        })
        .unwrap_or_else(|| "unknown".to_string());

    let hash = compute_file_hash(&path).unwrap_or_else(|_| "err".to_string());
    let hash_trimmed = hash.trim_end_matches('=');
    let hash_short = if hash_trimmed.len() > 6 {
        hash_trimmed[hash_trimmed.len() - 6..].to_string()
    } else {
        hash_trimmed.to_string()
    };

    Some(malkuth::info_page::BinaryInfo {
        name,
        path: path_str,
        compile_time,
        hash,
        hash_short,
    })
}

fn compute_file_hash(path: &std::path::Path) -> Result<String, Box<dyn std::error::Error>> {
    use sha2::Digest;
    use std::io::Read;
    let mut f = std::fs::File::open(path)?;
    let mut hasher = sha2::Sha256::new();
    let mut buf = [0u8; 8192];
    loop {
        let n = f.read(&mut buf)?;
        if n == 0 {
            break;
        }
        hasher.update(&buf[..n]);
    }
    let result = hasher.finalize();
    Ok(base32_encode(&result))
}

const BASE32_ALPHABET: &[u8] = b"ABCDEFGHIJKLMNOPQRSTUVWXYZ234567";

fn base32_encode(bytes: &[u8]) -> String {
    let mut out = String::new();
    let mut buf = 0u64;
    let mut bits = 0;
    for &b in bytes {
        buf = (buf << 8) | u64::from(b);
        bits += 8;
        while bits >= 5 {
            bits -= 5;
            let idx = ((buf >> bits) & 0x1f) as usize;
            out.push(BASE32_ALPHABET[idx] as char);
        }
    }
    if bits > 0 {
        let idx = ((buf << (5 - bits)) & 0x1f) as usize;
        out.push(BASE32_ALPHABET[idx] as char);
    }
    while out.len() % 8 != 0 {
        out.push('=');
    }
    out
}

/// Formats timestamps as local time `YYYY-MM-DD HH:MM:SS` (no timezone suffix),
/// matching the format used by sibling celestia-island CLIs (e.g. lagrange).
#[allow(dead_code)]
struct MalkuthTimer;

impl tracing_subscriber::fmt::time::FormatTime for MalkuthTimer {
    #[allow(dead_code)]
    fn format_time(&self, w: &mut tracing_subscriber::fmt::format::Writer<'_>) -> std::fmt::Result {
        write!(w, "{}", chrono::Local::now().format("%Y-%m-%d %H:%M:%S"))
    }
}

#[tokio::main]
async fn main() {
    // Intercept `malkuth mcp` before the watchdog arg parser runs: the
    // watchdog uses `-- <cmd>` positional args, which a clap subcommand would
    // conflict with, so we special-case the MCP server here.
    #[cfg(feature = "mcp")]
    {
        let mut args = std::env::args_os();
        let _ = args.next(); // program name
        if args.next().is_some_and(|a| a == "mcp") {
            if let Err(e) = malkuth::mcp::run().await {
                error!("{e}");
                std::process::exit(1);
            }
            return;
        }
    }

    // Intercept `malkuth daemon --config <path>` before the watchdog parser.
    #[cfg(all(feature = "cli", feature = "worker"))]
    {
        let mut args = std::env::args_os().skip(1);
        if args.next().is_some_and(|a| a == "daemon") {
            // Init tracing before running
            tracing_subscriber::fmt()
                .with_env_filter(
                    tracing_subscriber::EnvFilter::try_from_default_env()
                        .unwrap_or_else(|_| tracing_subscriber::EnvFilter::new("info")),
                )
                .with_timer(MalkuthTimer)
                .init();

            let config_path = args
                .find(|a| a == "--config")
                .and_then(|_| args.next())
                .or_else(|| {
                    args = std::env::args_os().skip(1);
                    let _ = args.next(); // skip "daemon"
                    args.find(|a| a == "-c").and_then(|_| args.next())
                });
            let config_path = match config_path {
                Some(p) => p.into_string().unwrap_or_default(),
                None => {
                    error!("daemon requires --config <path>");
                    std::process::exit(2);
                }
            };
            if let Err(e) = run_daemon(&config_path).await {
                error!("{e}");
                std::process::exit(1);
            }
            return;
        }
    }

    tracing_subscriber::fmt()
        .with_env_filter(
            tracing_subscriber::EnvFilter::try_from_default_env()
                .unwrap_or_else(|_| tracing_subscriber::EnvFilter::new("info")),
        )
        .with_timer(MalkuthTimer)
        .init();

    let args = Args::parse();
    if args.command.is_empty() {
        error!("no command given — usage: malkuth [OPTIONS] -- <cmd> [args...]");
        std::process::exit(2);
    }

    let proxy_spec = args.proxy.as_deref().map(|s| {
        ProxySpec::parse(s).unwrap_or_else(|e| {
            error!("{e}");
            std::process::exit(2);
        })
    });

    // Singleton lock — prevents duplicate instances on the same proxy port.
    if args.singleton {
        let port = proxy_spec.map(|s| s.public_port).unwrap_or(0);
        if port > 0 {
            match singleton::acquire(port) {
                Ok(_guard) => {
                    // Leak the guard — held for the lifetime of the process.
                    // On exit the OS releases the flock automatically.
                    std::mem::forget(_guard);
                    info!(port, "singleton lock acquired");
                }
                Err(e) => {
                    error!("{e}");
                    std::process::exit(1);
                }
            }
        } else {
            warn!("--singleton requires --proxy to be set; ignoring");
        }
    }

    // ── Self-update: spawn new binary with inherited fd, then drain ──
    #[cfg(unix)]
    if let Some(ref new_binary) = args.self_update {
        let proxy_fd = proxy_spec.as_ref().and_then(|_spec| {
            // We can't get the fd before binding. The self-update
            // will fork+exec from the daemon mode handler instead.
            info!("self-update requested for watchdog mode; forwarding to daemon handler");
            None::<i32>
        });
        let extra: Vec<String> = std::env::args().collect();
        match self_update::spawn_with_listen_fd(new_binary, &extra[1..], proxy_fd.unwrap_or(0)) {
            Ok(_child) => {
                info!("self-update: spawned new process; parent draining and exiting");
                return;
            }
            Err(e) => {
                error!(error = %e, "self-update spawn failed");
                std::process::exit(1);
            }
        }
    }

    // ── Handle inherited listener fd from self-update takeover ───
    #[cfg(unix)]
    if let Some(fd) = self_update::inherited_listener_fd() {
        info!(fd, "taking over inherited listener fd");
        use std::os::unix::io::FromRawFd;
        let std_listener = unsafe { std::net::TcpListener::from_raw_fd(fd) };
        std_listener.set_nonblocking(true).ok();
        let _tokio_listener = tokio::net::TcpListener::from_std(std_listener)
            .map_err(|e| error!(error = %e, "failed to create tokio listener from inherited fd"))
            .ok();
        info!("successfully took over inherited listener fd {}", fd);
    }

    let ports = match &proxy_spec {
        Some(spec) => assign_ports(
            spec.backend_ports().collect::<Vec<_>>().into_iter(),
            args.pod_count,
            spec.public_port,
        ),
        None => (0..args.pod_count).map(|i| (i, 0u16)).collect(),
    };

    let proxy_state = proxy_spec
        .map(|_spec| Arc::new(ProxyState::new(Duration::from_secs(args.sticky_ttl_secs))));
    if let Some(spec) = proxy_spec {
        info!(
            public = spec.public_port,
            range = %format!("{}-{}", spec.range_lo, spec.range_hi),
            pods = args.pod_count,
            "starting sticky reverse proxy"
        );
    }

    let manager = Arc::new(PodManager::new(
        args.host.clone(),
        args.port_env.clone(),
        args.command.clone(),
        proxy_state.clone(),
        ports,
        args.drain_secs,
    ));
    Arc::clone(&manager).run().await;

    if let Some(state) = proxy_state {
        if let Some(spec) = proxy_spec {
            let public: SocketAddr = format!("{}:{}", args.host, spec.public_port)
                .parse()
                .unwrap_or_else(|e| {
                    error!("invalid proxy bind address: {e}");
                    std::process::exit(2);
                });
            let proxy_type = args.proxy_type.clone();
            let _ipc_path = args.ipc_path.clone();
            tokio::spawn(async move {
                match proxy_type.as_str() {
                    #[cfg(feature = "ws")]
                    "ws" => {
                        if let Err(e) = ws_proxy::run_ws_proxy(public, state, HashMap::new()).await
                        {
                            error!(error = %e, "ws proxy stopped");
                        }
                    }
                    #[cfg(feature = "ipc")]
                    "ipc" => {
                        let path = _ipc_path.unwrap_or_else(|| "/tmp/malkuth-proxy.sock".into());
                        if let Err(e) = ipc_proxy::run_ipc_proxy(&path, state).await {
                            error!(error = %e, "ipc proxy stopped");
                        }
                    }
                    _ => {
                        if let Err(e) = proxy::run_proxy(public, state).await {
                            error!(error = %e, "tcp proxy stopped");
                        }
                    }
                }
            });
        }
    }

    if !args.watch.is_empty() {
        let mut rx = watcher::spawn(args.watch.clone(), args.debounce);
        let build_cmd = args.build.clone();
        let watch_paths = args.watch.clone();
        let pod_count = args.pod_count.max(1);
        let manager = Arc::clone(&manager);
        tokio::spawn(async move {
            let mut next_pod: usize = 0;
            while rx.recv().await.is_some() {
                // Run optional build command before restarting.
                // Only restart if the build actually produced changed output.
                if let Some(ref cmd) = build_cmd {
                    info!(cmd, "running build command");
                    let before = snapshot_mtimes(&watch_paths);
                    let status = tokio::process::Command::new("sh")
                        .arg("-c")
                        .arg(cmd)
                        .stdout(Stdio::inherit())
                        .stderr(Stdio::inherit())
                        .status()
                        .await;
                    match status {
                        Ok(s) if s.success() => {
                            let after = snapshot_mtimes(&watch_paths);
                            if mtimes_changed(&before, &after) {
                                info!(cmd, "build produced changes, proceeding with restart");
                            } else {
                                info!(cmd, "build produced no changes, skipping restart");
                                continue;
                            }
                        }
                        Ok(s) => {
                            warn!(cmd, code = %s, "build failed; skipping restart");
                            continue;
                        }
                        Err(e) => {
                            warn!(cmd, error = %e, "build command error; skipping restart");
                            continue;
                        }
                    }
                }
                let id = next_pod % pod_count;
                next_pod = next_pod.wrapping_add(1);
                info!(pod = id, "rolling restart triggered");
                manager.restart_one(id).await;
            }
        });
    }

    // ── Info page HTTP server (optional) ──────────────────────
    if let Some(info_port) = args.info_port {
        let addr: SocketAddr = format!("{}:{}", args.host, info_port)
            .parse()
            .unwrap_or_else(|e| {
                error!("invalid info-port bind address: {e}");
                std::process::exit(2);
            });
        let version = DEFAULT_VERSION.to_string();
        let watch = args
            .watch
            .iter()
            .map(|p| p.display().to_string())
            .collect::<Vec<_>>();
        let proxy = proxy_spec
            .as_ref()
            .map(|s| format!("0.0.0.0:{} → {}-{}", s.public_port, s.range_lo, s.range_hi));
        let show_details = !args.release;
        let status = if args.info_landing {
            malkuth::info_page::InfoStatus::Landing
        } else {
            malkuth::info_page::InfoStatus::Working
        };
        let command_str = args.command.first().map(|s| s.as_str()).unwrap_or("");
        let binaries = if args.info_landing {
            collect_binary_info(command_str).into_iter().collect()
        } else {
            vec![]
        };
        tokio::spawn(async move {
            let router = malkuth::info_page::info_router(
                version,
                status,
                if show_details { watch } else { vec![] },
                if show_details { proxy } else { None },
                binaries,
            );
            let listener = match tokio::net::TcpListener::bind(addr).await {
                Ok(l) => l,
                Err(e) => {
                    error!(addr = %addr, error = %e, "failed to bind info page listener");
                    return;
                }
            };
            info!(addr = %addr, "info page listening");
            if let Err(e) = axum::serve(listener, router).await {
                error!(error = %e, "info page server stopped");
            }
        });
    }

    info!("malkuth supervisor ready; press Ctrl-C to stop");
    signal::ctrl_c().await.ok();
    info!("shutdown signal received; exiting (child pods killed via kill_on_drop)");
}

#[cfg(all(feature = "cli", feature = "worker"))]
async fn run_daemon(config_path: &str) -> Result<(), String> {
    use malkuth::{DrainController, Supervisor, config::DaemonConfig};
    use std::time::Duration;

    let pid_file = {
        let cfg = DaemonConfig::from_file(config_path)?;
        cfg.daemon
            .pid_file
            .clone()
            .unwrap_or_else(|| "/tmp/malkuth-daemon.pid".into())
    };

    match acquire_daemon_lock(&pid_file) {
        Ok(()) => info!(%pid_file, "daemon lock acquired"),
        Err(e) => return Err(e),
    }

    // ── Signal sources ──────────────────────────────────────────
    let reload_notify = std::sync::Arc::new(tokio::sync::Notify::new());
    let (exit_tx, exit_rx) = tokio::sync::watch::channel(false);

    // SIGINT/SIGTERM → exit
    {
        let exit_tx = exit_tx.clone();
        tokio::spawn(async move {
            tokio::signal::ctrl_c().await.ok();
            info!("SIGINT received");
            let _ = exit_tx.send(true);
        });
    }
    #[cfg(unix)]
    {
        let exit_tx2 = exit_tx.clone();
        tokio::spawn(async move {
            let mut sig =
                tokio::signal::unix::signal(tokio::signal::unix::SignalKind::terminate()).ok();
            if let Some(sig) = sig.as_mut() {
                sig.recv().await;
            }
            info!("SIGTERM received");
            let _ = exit_tx2.send(true);
        });
    }
    // SIGHUP → reload
    #[cfg(unix)]
    {
        let reload2 = reload_notify.clone();
        tokio::spawn(async move {
            let mut sig =
                tokio::signal::unix::signal(tokio::signal::unix::SignalKind::hangup()).ok();
            loop {
                if let Some(sig) = sig.as_mut() {
                    sig.recv().await;
                } else {
                    break;
                }
                info!("SIGHUP received");
                reload2.notify_one();
            }
        });
    }

    // ── Supervision loop ────────────────────────────────────────
    loop {
        let cfg = DaemonConfig::from_file(config_path).unwrap_or_else(|e| {
            error!(config_path, %e, "config read failed");
            std::process::exit(1);
        });
        let daemon_host = cfg.daemon.host.clone();
        let max_restarts = cfg.daemon.rate_limit_max_restarts;
        let rate_window = Duration::from_secs(cfg.daemon.rate_limit_window_secs);
        let cooldown = Duration::from_secs(cfg.daemon.cooldown_secs);

        let specs = cfg.into_worker_specs();
        let service_list: Vec<_> = specs
            .iter()
            .map(|s| (s.id.clone(), s.program.clone()))
            .collect();
        let service_count = service_list.len();
        if specs.is_empty() {
            error!("config defines no [[services]]");
            std::process::exit(1);
        }

        let drain = DrainController::new();
        let drain_for_signal = drain.clone();

        // Per-iteration signal wiring: drain on reload or exit
        let reload2 = reload_notify.clone();
        let mut exit_rx2 = exit_rx.clone();
        tokio::spawn(async move {
            tokio::select! {
                _ = reload2.notified() => {}
                _ = exit_rx2.changed() => {}
            }
            drain_for_signal.begin_drain(malkuth::ShutdownKind::Graceful);
        });

        let supervisor = Supervisor::new(specs)
            .rate_limit(max_restarts, rate_window)
            .cooldown(cooldown);

        info!(services = service_count, host = %daemon_host, "malkuth daemon starting");
        for (id, program) in &service_list {
            info!(id = %id, program = %program, "worker registered");
        }

        let results = supervisor.run(drain).await;
        for info in &results {
            info!(id = %info.id, restarts = info.restart_count, "worker drained");
        }

        if *exit_rx.borrow() {
            info!("exit signal received, shutting down");
            break;
        }
        info!("reloading config and restarting workers...");
    }

    release_daemon_lock(&pid_file);
    Ok(())
}

#[cfg(all(feature = "cli", feature = "worker"))]
fn acquire_daemon_lock(pid_file: &str) -> Result<(), String> {
    use std::fs;
    use std::io::Write;

    if let Ok(contents) = fs::read_to_string(pid_file) {
        let old_pid: i32 = contents.trim().parse().unwrap_or(0);
        if old_pid > 0 {
            // Check if old process is still alive
            #[cfg(unix)]
            {
                unsafe {
                    if libc::kill(old_pid, 0) == 0 {
                        return Err(format!(
                            "daemon already running (pid={}), pid_file={}",
                            old_pid, pid_file
                        ));
                    }
                }
            }
            warn!(old_pid, "stale pid file, removing");
        }
    }

    // Write current PID
    let mut f = fs::File::create(pid_file)
        .map_err(|e| format!("cannot create pid file {}: {}", pid_file, e))?;
    let pid = std::process::id();
    write!(f, "{}", pid).map_err(|e| format!("cannot write pid file: {}", e))?;

    Ok(())
}

#[cfg(all(feature = "cli", feature = "worker"))]
fn release_daemon_lock(pid_file: &str) {
    std::fs::remove_file(pid_file).ok();
}
