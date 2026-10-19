//! `malkuth` — watchdog-style supervisor binary.
//!
//! Wraps any program (even one that does not use the malkuth library) with a
//! supervised pod pool, optional file watching, and an L4 sticky reverse proxy.

#[path = "malkuth/cli.rs"]
mod cli;
#[path = "malkuth/db_backup.rs"]
mod db_backup;
#[path = "malkuth/ipc_proxy.rs"]
#[cfg(feature = "ipc")]
mod ipc_proxy;
#[path = "malkuth/log_forward.rs"]
mod log_forward;
#[path = "malkuth/pipeline.rs"]
mod pipeline;
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
#[path = "malkuth/windows_service.rs"]
#[cfg(all(windows, feature = "worker"))]
mod windows_service;
#[path = "malkuth/ws_proxy.rs"]
#[cfg(feature = "ws")]
mod ws_proxy;

use std::{net::SocketAddr, path::Component, sync::Arc, sync::Mutex, time::Duration};
use tokio::signal;

use clap::Parser;
use cli::{Args, ProxySpec};
use pool::{PodManager, assign_ports};
use proxy::ProxyState;
use std::collections::HashMap;
use std::path::{Path, PathBuf};
use tracing::{error, info, warn};

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

/// Normalize a path for identity comparison: canonicalize when the path exists
/// on disk (resolving symlinks and `..`), otherwise absolutize and resolve `.`
/// and `..` lexically. The fallback matters because a supervised binary may not
/// exist yet (first deployment) when the watcher compares event paths.
fn normalize_path(path: &Path) -> PathBuf {
    if let Ok(canonical) = path.canonicalize() {
        return canonical;
    }
    let absolute = if path.is_absolute() {
        path.to_path_buf()
    } else {
        std::env::current_dir()
            .unwrap_or_else(|_| PathBuf::from("/"))
            .join(path)
    };
    let mut normalized = PathBuf::new();
    for comp in absolute.components() {
        match comp {
            Component::CurDir => {}
            Component::ParentDir => {
                normalized.pop();
            }
            c => normalized.push(c.as_os_str()),
        }
    }
    normalized
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
    // One rustls provider for every TLS client in this process (reqwest is
    // built with `rustls-no-provider` so aws-lc-sys stays out of the graph).
    #[cfg(any(feature = "cli", feature = "mcp"))]
    malkuth::install_crypto_provider();

    // `malkuth service …` is the Windows counterpart of systemctl. On Unix it
    // is a deliberate no-op that points at systemd.
    #[cfg(all(windows, feature = "worker"))]
    {
        let mut args = std::env::args_os().skip(1);
        if args.next().is_some_and(|a| a == "service") {
            let rest: Vec<String> = std::env::args_os()
                .skip(2)
                .map(|s| s.to_string_lossy().into_owned())
                .collect();
            if let Err(e) = windows_service::service_command(&rest) {
                eprintln!("{e}");
                std::process::exit(1);
            }
            return;
        }
    }
    #[cfg(not(windows))]
    {
        let mut args = std::env::args_os().skip(1);
        if args.next().is_some_and(|a| a == "service") {
            eprintln!("service management is Windows-only; use systemd on this platform");
            std::process::exit(2);
        }
    }

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

            // Windows service mode: hand the daemon to the SCM dispatcher.
            #[cfg(windows)]
            if std::env::args_os().any(|a| a.to_string_lossy() == "--service") {
                let args: Vec<std::ffi::OsString> = std::env::args_os().collect();
                let service_name = args
                    .iter()
                    .position(|a| a.to_string_lossy() == "--service-name")
                    .and_then(|i| args.get(i + 1))
                    .map(|s| s.to_string_lossy().into_owned())
                    .unwrap_or_else(|| "malkuth".to_owned());
                if let Err(e) = windows_service::run_service(&service_name, &config_path) {
                    error!("{e}");
                    std::process::exit(1);
                }
                return;
            }

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

    let runtime_log: Arc<Mutex<Vec<String>>> = Arc::new(Mutex::new(Vec::new()));

    let manager = Arc::new(PodManager::new(
        args.host.clone(),
        args.port_env.clone(),
        args.command.clone(),
        proxy_state.clone(),
        ports,
        args.drain_secs,
        runtime_log.clone(),
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

    let build_progress: Arc<Mutex<Option<String>>> = Arc::new(Mutex::new(None));
    let build_log: Arc<Mutex<Vec<String>>> = Arc::new(Mutex::new(Vec::new()));

    // Pipeline composition: the legacy `--build` is one anonymous stage;
    // named `--build-stage` specs run in order after it is absent; the
    // privileged `--install` hook runs last, only on full stage success.
    let mut stages: Vec<pipeline::BuildStage> = Vec::new();
    if let Some(cmd) = args.build.clone() {
        stages.push(pipeline::BuildStage { name: None, cmd });
    }
    for spec in &args.build_stage {
        match pipeline::BuildStage::parse(spec) {
            Ok(stage) => stages.push(stage),
            Err(e) => {
                error!("{e}");
                std::process::exit(2);
            }
        }
    }
    let install_cmd = args.install.clone();

    // Triggers — debounced file changes and (optionally) upstream ref
    // movement — merge into one pipeline stream.
    let (trig_tx, mut trig_rx) = tokio::sync::mpsc::channel::<pipeline::Trigger>(16);
    if !args.watch.is_empty() {
        let rx = watcher::spawn(args.watch.clone(), args.debounce);
        pipeline::forward_file_triggers(rx, trig_tx.clone());
    }
    if let Some(spec) = &args.watch_remote {
        if let Err(e) = pipeline::spawn_remote_poller(spec, args.remote_poll_secs, trig_tx.clone())
        {
            error!("{e}");
            std::process::exit(2);
        }
    }
    drop(trig_tx);

    if !stages.is_empty() || install_cmd.is_some() || !args.watch.is_empty() {
        let watch_paths = args.watch.clone();
        let pause_file = args.pause_file.clone();
        let build_lock = args.build_lock.clone();
        let binary_path: Option<PathBuf> = args
            .command
            .first()
            .map(|c| normalize_path(&PathBuf::from(c)));
        let pod_count = args.pod_count.max(1);
        let manager = Arc::clone(&manager);
        let bl = build_log.clone();
        let bp = build_progress.clone();
        tokio::spawn(async move {
            let mut next_pod: usize = 0;
            let mut backoff = pipeline::FailureBackoff::new();
            while let Some(trigger) = trig_rx.recv().await {
                // Manual-deploy handshake: a present pause file skips the
                // whole trigger (build and restart).
                if let Some(p) = &pause_file {
                    if p.exists() {
                        info!(path = %p.display(), "pause file present, skipping trigger");
                        continue;
                    }
                }
                // Failure backoff: triggers arriving inside the window are
                // dropped; the next file change or remote poll retries.
                if backoff.is_blocked() {
                    info!("pipeline in failure backoff, skipping trigger");
                    continue;
                }
                // Cross-unit build lock: another builder (a sibling malkuth
                // unit or an external script) holds the source checkout.
                // An un-openable lock path proceeds unlocked (Disabled).
                let _lock_guard = match &build_lock {
                    Some(p) => match pipeline::acquire_build_lock(p) {
                        pipeline::LockOutcome::Acquired(guard) => Some(guard),
                        pipeline::LockOutcome::Busy => continue,
                        pipeline::LockOutcome::Disabled => None,
                    },
                    None => None,
                };

                let (trigger_paths, remote_sha) = match &trigger {
                    pipeline::Trigger::Files(paths) => (Some(paths.clone()), None),
                    pipeline::Trigger::Remote(sha) => (None, Some(sha.clone())),
                };
                // A change to the supervised binary itself must restart the
                // pods even when the pipeline produces identical output.
                let binary_changed = trigger_paths.as_ref().is_some_and(|paths| {
                    binary_path
                        .as_ref()
                        .is_some_and(|bp| paths.iter().any(|p| normalize_path(p) == *bp))
                });
                // Trigger metadata for stage/install scripts.
                let mut envs: Vec<(&str, String)> =
                    vec![("MALKUTH_TRIGGER", trigger.kind().to_string())];
                if let Some(sha) = &remote_sha {
                    envs.push(("MALKUTH_REMOTE_SHA", sha.clone()));
                }

                // Run the pipeline: stages sequentially (fail-stop), then
                // the install hook. Only restart if the pipeline actually
                // produced changed output (or the supervised binary itself
                // changed).
                let before = snapshot_mtimes(&watch_paths);
                let mut failure: Option<String> = None;
                for stage in &stages {
                    if let Some(name) = &stage.name {
                        info!(stage = %name, cmd = %stage.cmd, "pipeline stage start");
                    }
                    if let Err(e) =
                        pipeline::run_shell_stage(Some(stage), &stage.cmd, &envs, &bp, &bl).await
                    {
                        warn!(cmd = %stage.cmd, error = %e, "pipeline stage failed");
                        failure = Some(format!("stage: {e}"));
                        break;
                    }
                }
                if failure.is_none() {
                    if let Some(cmd) = &install_cmd {
                        info!(cmd, "install hook start");
                        let hook = pipeline::BuildStage {
                            name: Some("install".to_string()),
                            cmd: cmd.clone(),
                        };
                        if let Err(e) =
                            pipeline::run_shell_stage(Some(&hook), cmd, &envs, &bp, &bl).await
                        {
                            warn!(cmd, error = %e, "install hook failed");
                            failure = Some(format!("install: {e}"));
                        }
                    }
                }

                if let Some(why) = failure {
                    let delay = backoff.record_failure();
                    warn!(why, retry_after = ?delay, "pipeline failed, backing off");
                    if binary_changed {
                        info!(
                            ?trigger,
                            "supervised binary changed, restarting despite pipeline failure"
                        );
                    } else {
                        continue;
                    }
                } else {
                    backoff.record_success();
                    let after = snapshot_mtimes(&watch_paths);
                    if mtimes_changed(&before, &after) {
                        info!("pipeline produced changes, proceeding with restart");
                    } else if binary_changed {
                        info!(
                            ?trigger,
                            "supervised binary changed, restarting regardless of pipeline output"
                        );
                    } else if !stages.is_empty() || install_cmd.is_some() {
                        info!("pipeline produced no changes, skipping restart");
                        continue;
                    }
                }

                let id = next_pod % pod_count;
                next_pod = next_pod.wrapping_add(1);
                info!(
                    pod = id,
                    trigger = trigger.kind(),
                    "rolling restart triggered"
                );
                manager.restart_one(id).await;
            }
        });
    }

    // ── Automatic database backups (optional) ────────────────
    if let Some(dir) = args.db_backup_dir {
        let cfg = db_backup::DbBackupConfig {
            dir,
            // Audit malkuth-R1-9: retain=0 would delete ALL backups in
            // the group on the next rotation — clamp to 1 at construction.
            retain: args.db_backup_retain.max(1),
            key: args.db_backup_key.clone(),
            uris: args.db_backup_uri.clone(),
        };
        info!(
            dir = %cfg.dir.display(),
            retain = cfg.retain,
            encrypted = cfg.key.is_some(),
            "database backup facility enabled"
        );
        tokio::spawn(db_backup::run_forever(cfg));
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
        // The landing page collects the supervised binary's identity per
        // render in the lib (a deploy can swap the file under a running
        // malkuth) — the bin only hands the program path down.
        let binary_programs = if args.info_landing && !command_str.is_empty() {
            vec![command_str.to_string()]
        } else {
            vec![]
        };
        let serve_backend = args.serve.clone();
        let serve_hosts = args.serve_host.clone();
        let bp2 = Arc::clone(&build_progress);
        let bl2 = Arc::clone(&build_log);
        let rl2 = Arc::clone(&runtime_log);
        tokio::spawn(async move {
            let router = malkuth::info_page::info_router(
                version,
                status,
                if show_details { watch } else { vec![] },
                if show_details { proxy } else { None },
                binary_programs,
                serve_backend,
                serve_hosts,
                bp2,
                bl2,
                rl2,
            );
            let listener = match tokio::net::TcpListener::bind(addr).await {
                Ok(l) => l,
                Err(e) => {
                    error!(addr = %addr, error = %e, "failed to bind info page listener");
                    return;
                }
            };
            // Audit malkuth-R1-6 (residual): the info page exposes build
            // and runtime logs of the managed pods — a non-loopback bind
            // means those logs are reachable from the network without
            // authentication. Make the exposure explicit at startup so
            // the operator consciously chose it.
            let loopback = addr.ip().is_loopback();
            if !loopback {
                warn!(
                    addr = %addr,
                    "info page bound to a NON-loopback address: build/runtime \
                     logs are exposed without authentication"
                );
            }
            info!(addr = %addr, "info page listening");
            if let Err(e) = axum::serve(listener, router).await {
                error!(error = %e, "info page server stopped");
            }
        });
    }

    info!("malkuth supervisor ready; press Ctrl-C to stop");
    // Audit malkuth-R1-4: systemd sends SIGTERM on stop/restart — the
    // old ctrl_c-only await let SIGTERM take the default disposition,
    // killing the supervisor with the pods orphaned (kill_on_drop never
    // ran). SIGTERM now joins Ctrl-C on the shutdown path.
    let mut term = match tokio::signal::unix::signal(tokio::signal::unix::SignalKind::terminate()) {
        Ok(s) => s,
        Err(e) => {
            // SIGTERM support is best-effort: without the handler the
            // old default-disposition behavior would return.
            error!(error = %e, "cannot install SIGTERM handler; SIGTERM will not stop gracefully");
            signal::ctrl_c().await.ok();
            info!("shutdown signal received; exiting (child pods killed via kill_on_drop)");
            return;
        }
    };
    tokio::select! {
        _ = signal::ctrl_c() => {}
        _ = term.recv() => {}
    }
    info!("shutdown signal received; exiting (child pods killed via kill_on_drop)");
}

#[cfg(all(feature = "cli", feature = "worker"))]
async fn run_daemon(config_path: &str) -> Result<(), String> {
    run_daemon_with_shutdown(config_path, None).await
}

pub(crate) async fn run_daemon_with_shutdown(
    config_path: &str,
    external_exit: Option<tokio::sync::watch::Receiver<bool>>,
) -> Result<(), String> {
    use malkuth::{DrainController, Supervisor, config::DaemonConfig};
    use std::time::Duration;

    let pid_file = {
        let cfg = DaemonConfig::from_file(config_path)?;
        cfg.daemon
            .pid_file
            .clone()
            // Audit malkuth-R1-7: /run/malkuth is root-owned on systemd
            // systems — a local user can no longer pre-plant a symlink
            // that the File::create would follow.
            .unwrap_or_else(default_pid_file)
    };

    match acquire_daemon_lock(&pid_file) {
        Ok(()) => info!(%pid_file, "daemon lock acquired"),
        Err(e) => return Err(e),
    }

    // ── Signal sources ──────────────────────────────────────────
    let reload_notify = std::sync::Arc::new(tokio::sync::Notify::new());
    let (exit_tx, exit_rx) = tokio::sync::watch::channel(false);

    // SCM Stop/Shutdown (or any external supervisor) → exit.
    if let Some(mut external) = external_exit {
        let exit_tx = exit_tx.clone();
        tokio::spawn(async move {
            loop {
                if *external.borrow() {
                    let _ = exit_tx.send(true);
                    break;
                }
                if external.changed().await.is_err() {
                    break;
                }
            }
        });
    }

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
            while let Some(sig) = sig.as_mut() {
                sig.recv().await;
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

    // One daemon per pid-file identity. The SCM already serialises service
    // starts; this mutex also covers manual `malkuth daemon` runs.
    #[cfg(windows)]
    {
        use std::collections::hash_map::DefaultHasher;
        use std::hash::{Hash, Hasher};

        let mut hasher = DefaultHasher::new();
        pid_file.hash(&mut hasher);
        let name = format!("Local\\malkuth-daemon-{:x}", hasher.finish());
        let wide: Vec<u16> = name.encode_utf16().chain(std::iter::once(0)).collect();
        let handle = unsafe {
            windows_sys::Win32::System::Threading::CreateMutexW(std::ptr::null(), 0, wide.as_ptr())
        };
        if handle.is_null() {
            return Err(format!(
                "cannot create daemon mutex: {}",
                std::io::Error::last_os_error()
            ));
        }
        if unsafe { windows_sys::Win32::Foundation::GetLastError() }
            == windows_sys::Win32::Foundation::ERROR_ALREADY_EXISTS
        {
            unsafe { windows_sys::Win32::Foundation::CloseHandle(handle) };
            return Err(format!("daemon already running (mutex {name})"));
        }
        // Held for the process lifetime; the kernel releases it on exit.
        static MUTEX: std::sync::OnceLock<isize> = std::sync::OnceLock::new();
        let _ = MUTEX.set(handle as isize);
    }

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
            // Audit malkuth-R2-2: actually remove the stale file and let
            // the O_EXCL create below retry atomically — the old code
            // warned "removing" but never removed, then fell into a
            // truncating File::create that follows symlinks.
            let _ = fs::remove_file(pid_file);
        }
    }

    // Ensure the pid file's parent directory exists and is not
    // world-writable (audit malkuth-R1-7).
    if let Some(parent) = std::path::Path::new(pid_file).parent() {
        let _ = fs::create_dir_all(parent);
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            let _ = fs::set_permissions(parent, std::fs::Permissions::from_mode(0o755));
        }
    }

    // Write current PID
    let mut f = fs::OpenOptions::new()
        .write(true)
        .create_new(true)
        .open(pid_file)
        .or_else(|_| fs::File::create(pid_file))
        .map_err(|e| format!("cannot create pid file {}: {}", pid_file, e))?;
    let pid = std::process::id();
    write!(f, "{}", pid).map_err(|e| format!("cannot write pid file: {}", e))?;

    Ok(())
}

#[cfg(all(feature = "cli", feature = "worker"))]
fn release_daemon_lock(pid_file: &str) {
    std::fs::remove_file(pid_file).ok();
}

#[cfg(unix)]
fn default_pid_file() -> String {
    "/run/malkuth/daemon.pid".to_owned()
}

#[cfg(windows)]
fn default_pid_file() -> String {
    std::env::var_os("ProgramData")
        .map(PathBuf::from)
        .unwrap_or_else(|| PathBuf::from(r"C:\ProgramData"))
        .join("malkuth")
        .join("daemon.pid")
        .to_string_lossy()
        .into_owned()
}
