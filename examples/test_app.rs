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
//! - `ws-echo` — a raw WebSocket echo server (hand-rolled framing, no deps).
//!   `GET /ws` upgrades and echoes every text/binary frame back; `GET /readyz`
//!   answers 200 so the malkuth probe counts it as up; any other upgrade
//!   handshake gets a 404 (used to assert pass-through semantics).
//!
//! Run: `cargo run --example test_app --features tcp,worker,signals -- <mode> [args]`

use std::{
    env,
    time::{Duration, Instant},
};
use tokio::{
    io::{AsyncBufReadExt, AsyncReadExt, AsyncWriteExt, BufReader},
    net::{TcpListener, TcpStream},
    task::JoinHandle,
};

use malkuth::{
    DrainController, RestartPolicy, ShutdownKind,
    worker::{Supervisor, WorkerSpec},
};

#[tokio::main]
async fn main() {
    let args: Vec<String> = env::args().collect();
    let mode = args.get(1).map(String::as_str).unwrap_or("");
    match mode {
        "worker" => worker().await,
        "supervise" => supervise(pods(&args), port_base(&args)).await,
        "rolling" => rolling(pods(&args), port_base(&args)).await,
        "ws-echo" => ws_echo().await,
        other => {
            eprintln!(
                "usage: malkuth-test-app worker | supervise --pods N --port-base B | rolling --pods N --port-base B | ws-echo"
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

// ── ws-echo mode ─────────────────────────────────────────────────

const WS_GUID: &[u8] = b"258EAFA5-E914-47DA-95CA-C5AB0DC85B11";

async fn ws_echo() {
    let port: u16 = env::var("PORT")
        .ok()
        .and_then(|p| p.parse().ok())
        .expect("PORT env required");
    eprintln!("WS_ECHO_READY port={port}");
    let listener = TcpListener::bind(("127.0.0.1", port))
        .await
        .expect("bind failed");
    loop {
        let (sock, _peer) = match listener.accept().await {
            Ok(v) => v,
            Err(e) => {
                eprintln!("WS_ECHO_ACCEPT_FAIL error={e}");
                continue;
            }
        };
        tokio::spawn(async move {
            if let Err(e) = handle_ws_echo(sock).await {
                eprintln!("WS_ECHO_ERR {e}");
            }
        });
    }
}

async fn handle_ws_echo(mut stream: TcpStream) -> std::io::Result<()> {
    let head = read_until_double_crlf(&mut stream, 64 * 1024).await?;
    let text = String::from_utf8_lossy(&head);
    let mut lines = text.split("\r\n");
    let request_line = lines.next().unwrap_or("");
    let path = request_line.split_whitespace().nth(1).unwrap_or("/");
    let headers: Vec<(String, String)> = lines
        .filter_map(|l| l.split_once(':'))
        .map(|(n, v)| (n.trim().to_ascii_lowercase(), v.trim().to_string()))
        .collect();

    let is_upgrade = headers.iter().any(|(n, v)| {
        n == "connection"
            && v.split(',')
                .any(|tok| tok.trim().eq_ignore_ascii_case("upgrade"))
    }) && headers.iter().any(|(n, _)| n == "upgrade");

    if !is_upgrade {
        // Non-upgrade traffic (e.g. the malkuth `/readyz` probe) counts as
        // a healthy plain-HTTP answer.
        stream
            .write_all(b"HTTP/1.1 200 OK\r\nContent-Length: 2\r\nConnection: close\r\n\r\nok")
            .await?;
        return Ok(());
    }
    if !path.eq("/ws") && !path.starts_with("/api/rpc") {
        // Upgrade handshakes to unknown paths are refused with a real HTTP
        // error that the front door must pass through untouched.
        stream
            .write_all(
                b"HTTP/1.1 404 Not Found\r\nContent-Length: 2\r\nConnection: close\r\n\r\nno",
            )
            .await?;
        return Ok(());
    }

    let key = headers
        .iter()
        .find(|(n, _)| n == "sec-websocket-key")
        .map(|(_, v)| v.clone())
        .unwrap_or_default();
    let accept = ws_accept(&key);
    let response = format!(
        "HTTP/1.1 101 Switching Protocols\r\nUpgrade: websocket\r\nConnection: Upgrade\r\nSec-WebSocket-Accept: {accept}\r\n\r\n"
    );
    stream.write_all(response.as_bytes()).await?;
    stream.flush().await?;

    loop {
        let Some((fin, opcode, payload)) = read_ws_frame(&mut stream).await? else {
            return Ok(());
        };
        match opcode {
            0x1 | 0x2 => write_ws_frame(&mut stream, fin, opcode, &payload).await?, // text/binary echo
            0x8 => {
                write_ws_frame(&mut stream, true, 0x8, &payload).await?; // close handshake
                return Ok(());
            }
            0x9 => write_ws_frame(&mut stream, true, 0xA, &payload).await?, // ping → pong
            _ => {}
        }
    }
}

/// Read bytes until `\r\n\r\n` (or the cap), returning everything read.
async fn read_until_double_crlf(stream: &mut TcpStream, cap: usize) -> std::io::Result<Vec<u8>> {
    let mut buf: Vec<u8> = Vec::with_capacity(1024);
    let mut byte = [0u8; 1];
    loop {
        if buf.len() >= cap {
            return Err(std::io::Error::new(
                std::io::ErrorKind::InvalidData,
                "request head exceeds cap",
            ));
        }
        if stream.read(&mut byte).await? == 0 {
            return Err(std::io::Error::new(
                std::io::ErrorKind::UnexpectedEof,
                "peer closed mid-head",
            ));
        }
        buf.push(byte[0]);
        if buf.ends_with(b"\r\n\r\n") {
            return Ok(buf);
        }
    }
}

/// Read one client frame: `(fin, opcode, unmasked payload)`.
async fn read_ws_frame(stream: &mut TcpStream) -> std::io::Result<Option<(bool, u8, Vec<u8>)>> {
    let mut b = [0u8; 1];
    if stream.read(&mut b).await? == 0 {
        return Ok(None);
    }
    let fin = b[0] & 0x80 != 0;
    let opcode = b[0] & 0x0F;
    if stream.read(&mut b).await? == 0 {
        return Err(std::io::Error::new(
            std::io::ErrorKind::UnexpectedEof,
            "truncated frame header",
        ));
    }
    let masked = b[0] & 0x80 != 0;
    let mut len = (b[0] & 0x7F) as u64;
    if len == 126 {
        let mut ext = [0u8; 2];
        stream.read_exact(&mut ext).await?;
        len = u64::from(u16::from_be_bytes(ext));
    } else if len == 127 {
        let mut ext = [0u8; 8];
        stream.read_exact(&mut ext).await?;
        len = u64::from_be_bytes(ext);
    }
    let len = usize::try_from(len).map_err(|_| std::io::Error::other("frame too large"))?;
    let mut mask = [0u8; 4];
    if masked {
        stream.read_exact(&mut mask).await?;
    }
    let mut payload = vec![0u8; len];
    stream.read_exact(&mut payload).await?;
    if masked {
        for (i, byte) in payload.iter_mut().enumerate() {
            *byte ^= mask[i % 4];
        }
    }
    Ok(Some((fin, opcode, payload)))
}

/// Write one server frame (never masked).
async fn write_ws_frame(
    stream: &mut TcpStream,
    fin: bool,
    opcode: u8,
    payload: &[u8],
) -> std::io::Result<()> {
    let mut header = vec![(if fin { 0x80 } else { 0x00 }) | (opcode & 0x0F)];
    if payload.len() < 126 {
        header.push(payload.len() as u8);
    } else if payload.len() <= u16::MAX as usize {
        header.push(126);
        header.extend_from_slice(&(payload.len() as u16).to_be_bytes());
    } else {
        header.push(127);
        header.extend_from_slice(&(payload.len() as u64).to_be_bytes());
    }
    stream.write_all(&header).await?;
    stream.write_all(payload).await?;
    stream.flush().await
}

/// `base64(sha1(key + GUID))` — the RFC 6455 Sec-WebSocket-Accept.
fn ws_accept(key: &str) -> String {
    let mut input = key.as_bytes().to_vec();
    input.extend_from_slice(WS_GUID);
    base64_encode(&sha1(&input))
}

fn sha1(data: &[u8]) -> [u8; 20] {
    let mut h: [u32; 5] = [
        0x6745_2301,
        0xEFCD_AB89,
        0x98BA_DCFE,
        0x1032_5476,
        0xC3D2_E1F0,
    ];
    let bit_len = (data.len() as u64) * 8;
    let mut msg = data.to_vec();
    msg.push(0x80);
    while msg.len() % 64 != 56 {
        msg.push(0);
    }
    msg.extend_from_slice(&bit_len.to_be_bytes());
    for chunk in msg.chunks(64) {
        let mut w = [0u32; 80];
        for (i, word) in chunk.chunks(4).enumerate() {
            w[i] = u32::from_be_bytes([word[0], word[1], word[2], word[3]]);
        }
        for i in 16..80 {
            w[i] = (w[i - 3] ^ w[i - 8] ^ w[i - 14] ^ w[i - 16]).rotate_left(1);
        }
        let (mut a, mut b, mut c, mut d, mut e) = (h[0], h[1], h[2], h[3], h[4]);
        for (i, &wi) in w.iter().enumerate() {
            let (f, k) = match i {
                0..=19 => ((b & c) | (!b & d), 0x5A82_7999),
                20..=39 => (b ^ c ^ d, 0x6ED9_EBA1),
                40..=59 => ((b & c) | (b & d) | (c & d), 0x8F1B_BCDC),
                _ => (b ^ c ^ d, 0xCA62_C1D6),
            };
            let tmp = a
                .rotate_left(5)
                .wrapping_add(f)
                .wrapping_add(e)
                .wrapping_add(k)
                .wrapping_add(wi);
            e = d;
            d = c;
            c = b.rotate_left(30);
            b = a;
            a = tmp;
        }
        h[0] = h[0].wrapping_add(a);
        h[1] = h[1].wrapping_add(b);
        h[2] = h[2].wrapping_add(c);
        h[3] = h[3].wrapping_add(d);
        h[4] = h[4].wrapping_add(e);
    }
    let mut out = [0u8; 20];
    for (i, hh) in h.iter().enumerate() {
        out[i * 4..i * 4 + 4].copy_from_slice(&hh.to_be_bytes());
    }
    out
}

fn base64_encode(data: &[u8]) -> String {
    const CHARS: &[u8] = b"ABCDEFGHIJKLMNOPQRSTUVWXYZabcdefghijklmnopqrstuvwxyz0123456789+/";
    let mut out = String::with_capacity(data.len().div_ceil(3) * 4);
    for chunk in data.chunks(3) {
        let b0 = chunk[0] as u32;
        let b1 = if chunk.len() > 1 { chunk[1] as u32 } else { 0 };
        let b2 = if chunk.len() > 2 { chunk[2] as u32 } else { 0 };
        let n = (b0 << 16) | (b1 << 8) | b2;
        out.push(CHARS[(n >> 18) as usize] as char);
        out.push(CHARS[((n >> 12) & 63) as usize] as char);
        if chunk.len() > 1 {
            out.push(CHARS[((n >> 6) & 63) as usize] as char);
        }
        if chunk.len() > 2 {
            out.push(CHARS[(n & 63) as usize] as char);
        }
    }
    while out.len() % 4 != 0 {
        out.push('=');
    }
    out
}
