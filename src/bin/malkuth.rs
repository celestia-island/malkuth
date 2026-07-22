//! `malkuth` — watchdog-style supervisor binary.
//!
//! Wraps any program (even one that does not use the malkuth library) with a
//! supervised pod pool, optional file watching, and an L4 sticky reverse proxy.

#[path = "malkuth/cli.rs"]
mod cli;
#[path = "malkuth/pool.rs"]
mod pool;
#[path = "malkuth/proxy.rs"]
mod proxy;
#[path = "malkuth/singleton.rs"]
mod singleton;
#[path = "malkuth/watcher.rs"]
mod watcher;

use std::{net::SocketAddr, sync::Arc, time::Duration};
use tokio::signal;

use clap::Parser;
use cli::{Args, ProxySpec};
use pool::{PodManager, assign_ports};
use proxy::{ProxyState, run_proxy};
use tracing::{error, info, warn};

/// Formats timestamps as local time `YYYY-MM-DD HH:MM:SS` (no timezone suffix),
/// matching the format used by sibling celestia-island CLIs (e.g. lagrange).
struct MalkuthTimer;

impl tracing_subscriber::fmt::time::FormatTime for MalkuthTimer {
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
            tokio::spawn(async move {
                if let Err(e) = run_proxy(public, state).await {
                    error!(error = %e, "proxy stopped");
                }
            });
        }
    }

    if !args.watch.is_empty() {
        let mut rx = watcher::spawn(args.watch.clone());
        let manager = Arc::clone(&manager);
        tokio::spawn(async move {
            let mut next_pod: usize = 0;
            while rx.recv().await.is_some() {
                let pod_count = args.pod_count.max(1);
                let id = next_pod % pod_count;
                next_pod = next_pod.wrapping_add(1);
                info!(pod = id, "rolling restart triggered by file change");
                manager.restart_one(id).await;
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

    let cfg = DaemonConfig::from_file(config_path)?;

    let pid_file = cfg.daemon.pid_file.clone()
        .unwrap_or_else(|| "/tmp/malkuth-daemon.pid".into());

    // ── Singleton guard ─────────────────────────────────────────
    match acquire_daemon_lock(&pid_file) {
        Ok(()) => info!(%pid_file, "daemon lock acquired"),
        Err(e) => return Err(e),
    }

    let service_count = cfg.services.len();
    let service_list: Vec<_> = cfg.services.iter().map(|s| (s.id.clone(), s.program.clone())).collect();
    let daemon_host = cfg.daemon.host.clone();
    let max_restarts = cfg.daemon.rate_limit_max_restarts;
    let rate_window = Duration::from_secs(cfg.daemon.rate_limit_window_secs);
    let cooldown = Duration::from_secs(cfg.daemon.cooldown_secs);

    let specs = cfg.into_worker_specs();

    if specs.is_empty() {
        return Err("config defines no [[services]]".into());
    }

    let drain = DrainController::new();

    // Wire Ctrl-C to begin drain.
    let drain_sig = drain.clone();
    tokio::spawn(async move {
        tokio::signal::ctrl_c().await.ok();
        drain_sig.begin_drain(malkuth::ShutdownKind::Graceful);
    });

    let supervisor = Supervisor::new(specs)
        .rate_limit(max_restarts, rate_window)
        .cooldown(cooldown);

    info!(
        services = service_count,
        host = %daemon_host,
        "malkuth daemon starting"
    );

    for (id, program) in &service_list {
        info!(id = %id, program = %program, "worker registered");
    }

    let results = supervisor.run(drain).await;

    // ── Cleanup ────────────────────────────────────────────────
    release_daemon_lock(&pid_file);

    for info in &results {
        warn!(
            id = %info.id,
            status = ?info.status,
            restarts = info.restart_count,
            "worker stopped"
        );
    }

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
    write!(f, "{}", pid)
        .map_err(|e| format!("cannot write pid file: {}", e))?;

    Ok(())
}

#[cfg(all(feature = "cli", feature = "worker"))]
fn release_daemon_lock(pid_file: &str) {
    std::fs::remove_file(pid_file).ok();
}
