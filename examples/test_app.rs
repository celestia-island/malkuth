//! Internal test/reference binary for malkuth.
//!
//! Three modes (parsed from argv, no clap to stay dep-light):
//!
//! - `worker` — a single supervised unit. Listens on `$PORT`, reports `$GEN`,
//!   speaks a tiny line protocol (`ping`→`pong`, `health`→`port=..;gen=..;pid=..`,
//!   `crash`→exit(1)). This is what gets replicated/wrapped.
//! - `supervise --pods N --port-base B` — uses `malkuth::worker::Supervisor` to
//!   run N copies of itself (self-replication); OTP restart on crash.
//! - `rolling --pods N --port-base B` — runs gen-0 pods, then performs a
//!   *gradual* gray update to gen-1 (one pod at a time: bring up new, drain old)
//!   using a per-pod `DrainController` + `Supervisor`.
//!
//! Run: `cargo run --example test_app --features tcp,worker,signals -- <mode> [args]`

use std::env;
use std::time::{Duration, Instant};

use malkuth::worker::{Supervisor, WorkerSpec};
use malkuth::{DrainController, RestartPolicy, ShutdownKind};
use tokio::io::{AsyncBufReadExt, AsyncWriteExt, BufReader};
use tokio::net::{TcpListener, TcpStream};
use tokio::task::JoinHandle;

#[tokio::main]
async fn main() {
    let args: Vec<String> = env::args().collect();
    let mode = args.get(1).map(String::as_str).unwrap_or("");
    match mode {
        "worker" => worker().await,
        "supervise" => supervise(pods(&args), port_base(&args)).await,
        "rolling" => rolling(pods(&args), port_base(&args)).await,
        other => {
            eprintln!(
                "usage: malkuth-test-app worker | supervise --pods N --port-base B | rolling --pods N --port-base B"
            );
            eprintln!("  (got: {other:?})");
            std::process::exit(2);
        }
    }
}

fn pods(args: &[String]) -> usize {
    flag(args, "--pods")
        .and_then(|v| v.parse().ok())
        .unwrap_or(3)
}
fn port_base(args: &[String]) -> u16 {
    flag(args, "--port-base")
        .and_then(|v| v.parse().ok())
        .unwrap_or(0)
}
fn flag<'a>(args: &'a [String], name: &str) -> Option<&'a str> {
    let mut it = args.iter();
    while let Some(a) = it.next() {
        if let Some(rest) = a.strip_prefix(name) {
            if rest.is_empty() {
                return it.next().map(String::as_str);
            }
            if let Some(v) = rest.strip_prefix('=') {
                return Some(v);
            }
        }
    }
    None
}

// ── worker mode ────────────────────────────────────────────────

async fn worker() {
    let port: u16 = env::var("PORT")
        .ok()
        .and_then(|p| p.parse().ok())
        .expect("PORT env required");
    let generation: u64 = env::var("GEN")
        .ok()
        .and_then(|g| g.parse().ok())
        .unwrap_or(0);
    eprintln!(
        "WORKER_READY port={port} gen={generation} pid={}",
        std::process::id()
    );
    let listener = match TcpListener::bind(("127.0.0.1", port)).await {
        Ok(l) => l,
        Err(e) => {
            eprintln!("WORKER_BIND_FAIL port={port} error={e}");
            std::process::exit(1);
        }
    };
    loop {
        let (sock, _peer) = match listener.accept().await {
            Ok(v) => v,
            Err(e) => {
                eprintln!("WORKER_ACCEPT_FAIL error={e}");
                continue;
            }
        };
        tokio::spawn(async move {
            handle_client(sock, port, generation).await;
        });
    }
}

async fn handle_client(stream: TcpStream, port: u16, generation: u64) {
    let mut reader = BufReader::new(stream);
    let mut line = String::new();
    loop {
        line.clear();
        let n = match reader.read_line(&mut line).await {
            Ok(n) => n,
            Err(_) => return,
        };
        if n == 0 {
            return;
        }
        let cmd = line.trim();
        let reply: Vec<u8> = match cmd {
            "ping" => b"pong\n".to_vec(),
            "health" => {
                format!("port={port};gen={generation};pid={}\n", std::process::id()).into_bytes()
            }
            "crash" => {
                eprintln!("WORKER_CRASH port={port} (requested)");
                std::process::exit(1);
            }
            other => format!("err: unknown command {other:?}\n").into_bytes(),
        };
        if reader.get_mut().write_all(&reply).await.is_err() {
            return;
        }
    }
}

// ── supervise mode ─────────────────────────────────────────────

async fn supervise(pods_n: usize, port_base: u16) {
    let exe = env::current_exe().expect("current_exe");
    let mut specs = Vec::new();
    for i in 0..pods_n {
        let port = port_base + 1 + i as u16;
        specs.push(
            WorkerSpec::new(format!("w{i}"), "app", exe.to_string_lossy().to_string())
                .args(["worker"])
                .env("PORT", port.to_string())
                .env("GEN", "0")
                .policy(RestartPolicy::Permanent),
        );
    }
    eprintln!("SUPERVISE_START pods={pods_n} port_base={port_base}");
    let drain = DrainController::new();
    let sup = Supervisor::new(specs);
    // Spawn the supervisor so it isn't dropped by select! (kill_on_drop would
    // kill every child instantly). Signal triggers drain, then we await the
    // supervisor to let it finish draining.
    let drain_for_sup = drain.clone();
    let sup_handle = tokio::spawn(async move { sup.run(drain_for_sup).await });
    tokio::signal::ctrl_c().await.ok();
    eprintln!("SUPERVISE_STOP signal; draining");
    drain.begin_drain(ShutdownKind::Graceful);
    let infos = sup_handle.await.expect("supervisor task panicked");
    for info in infos {
        eprintln!("SUPERVISE_FINAL {info:?}");
    }
    eprintln!("SUPERVISE_EXIT");
}

// ── rolling mode (gradual gray update) ─────────────────────────

struct Pod {
    port: u16,
    ctrl: DrainController,
    task: JoinHandle<()>,
}

fn spawn_pod(exe: &str, port: u16, generation: u64) -> Pod {
    let spec = WorkerSpec::new(format!("pod-{port}"), "app", exe.to_string())
        .args(["worker"])
        .env("PORT", port.to_string())
        .env("GEN", generation.to_string())
        .policy(RestartPolicy::Permanent);
    let ctrl = DrainController::new();
    let run_ctrl = ctrl.clone();
    let task = tokio::spawn(async move {
        let infos = Supervisor::new(vec![spec]).run(run_ctrl).await;
        for info in infos {
            eprintln!("POD_FINAL {info:?}");
        }
    });
    Pod { port, ctrl, task }
}

async fn drain_pod(pod: Pod) {
    pod.ctrl.begin_drain(ShutdownKind::Graceful);
    let _ = pod.task.await;
}

async fn wait_healthy(port: u16, deadline: Duration) -> bool {
    let end = Instant::now() + deadline;
    while Instant::now() < end {
        if TcpStream::connect(("127.0.0.1", port)).await.is_ok() {
            return true;
        }
        tokio::time::sleep(Duration::from_millis(50)).await;
    }
    false
}

async fn rolling(pods_n: usize, port_base: u16) {
    let exe = env::current_exe().expect("current_exe");
    let exe = exe.to_string_lossy().to_string();
    let gen0_ports: Vec<u16> = (0..pods_n).map(|i| port_base + 1 + i as u16).collect();
    let gen1_ports: Vec<u16> = (0..pods_n)
        .map(|i| port_base + 1 + pods_n as u16 + i as u16)
        .collect();

    // gen-0 up
    let mut gen0: Vec<Pod> = Vec::new();
    for &p in &gen0_ports {
        gen0.push(spawn_pod(&exe, p, 0));
    }
    for &p in &gen0_ports {
        if !wait_healthy(p, Duration::from_secs(10)).await {
            eprintln!("ROLLING_FAIL gen0 port={p} not healthy");
            std::process::exit(1);
        }
    }
    eprintln!("ROLLING_GEN0_READY ports={gen0_ports:?}");

    // gradual update: bring up each gen-1 pod, then drain the matching gen-0 pod
    let mut gen1: Vec<Pod> = Vec::new();
    for (i, &p1) in gen1_ports.iter().enumerate() {
        gen1.push(spawn_pod(&exe, p1, 1));
        if !wait_healthy(p1, Duration::from_secs(10)).await {
            eprintln!("ROLLING_FAIL gen1 port={p1} not healthy");
            std::process::exit(1);
        }
        drain_pod(gen0.remove(0)).await;
        eprintln!("ROLLING_STEP {i} gen1={p1} up, gen0 drained");
    }
    let gen1_serving: Vec<u16> = gen1.iter().map(|p| p.port).collect();
    eprintln!("ROLLING_DONE gen1 serving ports={gen1_serving:?}");

    _ = tokio::signal::ctrl_c().await;
    eprintln!("ROLLING_STOP signal");
    for pod in gen1 {
        drain_pod(pod).await;
    }
}
