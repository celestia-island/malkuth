use axum::{
    Router,
    extract::Request,
    http::{StatusCode, header},
    response::{Html, IntoResponse, Response},
    routing::any,
};
use hyper::upgrade::OnUpgrade;
use hyper_util::rt::TokioIo;
use std::collections::HashMap;
use std::sync::LazyLock;
use std::time::Duration;
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::net::TcpStream;

include!(concat!(env!("OUT_DIR"), "/landing_page_html.rs"));

const LOGO_BYTES: &[u8] = include_bytes!("info_page/logo.webp");

/// The exact prefix `build.rs` emits when the SPA bundle was never built on
/// this host. Any other content means a real single-file SPA is embedded.
const SPA_SENTINEL: &str = "<html><body><h1>Malkuth</h1>";

/// Is a real SPA bundle embedded (vs. the build.rs sentinel page)?
fn spa_available() -> bool {
    !LANDING_PAGE_HTML.starts_with(SPA_SENTINEL)
}

fn base64_encode(bytes: &[u8]) -> String {
    const CHARS: &[u8] = b"ABCDEFGHIJKLMNOPQRSTUVWXYZabcdefghijklmnopqrstuvwxyz0123456789+/";
    let mut out = String::with_capacity(bytes.len().div_ceil(3) * 4);
    for chunk in bytes.chunks(3) {
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

static I18N_DATA: LazyLock<HashMap<String, HashMap<String, String>>> = LazyLock::new(|| {
    let raw: HashMap<String, HashMap<String, String>> =
        serde_json::from_str(include_str!("info_page/i18n/all.json"))
            .expect("Failed to parse i18n JSON");
    raw
});

include!(concat!(env!("OUT_DIR"), "/fallback_page_html.rs"));

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum BackendState {
    Unknown,
    /// Backend is reachable and ready to receive traffic.
    Up,
    /// Backend is reachable but explicitly reports not-ready
    /// (e.g. `GET /readyz` → 503 while starting up or draining
    /// for a rolling restart).
    NotReady,
    /// Backend is unreachable (TCP refused / timeout / non-HTTP).
    Down,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum InfoStatus {
    /// All backends healthy, services running normally.
    Ready,
    /// Backend is restarting / starting / building.
    Working,
    /// Landing page mode: show binary details with auto-redirect countdown.
    Landing,
}

/// Information about a supervised binary for the landing page.
///
/// `hash`/`hash_short` carry the family build identity
/// (`<branch>:<hash7>`, e.g. `master:c350cb4`) — the same line the Web UI
/// status pill prints — whenever the binary itself states one (the
/// `plana_build_info` version line baked into family binaries at build
/// time; see [`collect_binary_info`]). Binaries without the convention
/// keep the legacy content fingerprint (SHA-256, base32, last 6 chars).
#[derive(Debug, Clone, serde::Serialize)]
pub struct BinaryInfo {
    pub name: String,
    pub path: String,
    pub compile_time: String,
    pub hash: String,
    #[serde(rename = "hash_short")]
    pub hash_short: String,
}

/// One cached collect: the binary's (mtime, size) when it was resolved,
/// with the resulting info — a stat match serves the memo without reading
/// the file.
type BinaryInfoMemo = (std::time::SystemTime, u64, BinaryInfo);

/// mtime+size-keyed memo of the last collect per binary: a landing-page
/// render re-resolves the identity per request (deploys swap the watched
/// binary without restarting malkuth — a startup-only snapshot would go
/// stale forever), while the fallback path hashes the whole file and must
/// not run per render for a 60 MB binary.
static BINARY_INFO_CACHE: LazyLock<std::sync::Mutex<HashMap<String, BinaryInfoMemo>>> =
    LazyLock::new(|| std::sync::Mutex::new(HashMap::new()));

/// Collect metadata for a supervised binary: name, compile timestamp and
/// the family build identity (`<branch>:<hash7>`) stated by the binary
/// itself, falling back to the SHA-256 content fingerprint.
///
/// The identity is read out of the binary's own bytes: family builds bake
/// the `plana_build_info` version line (`<base> <branch>:<hash7..40>`,
/// both `:` and the legacy `::` separator parse) into `.rodata` via
/// `env!("VERSION")`, so the deployed file is the single source of truth —
/// no deploy-side bookkeeping to drift. Registry-built dependency crates
/// resolve to non-hex markers (`detached:unknown`) and never match; a
/// binary carrying two DIFFERENT identities is ambiguous and falls back
/// rather than guessing.
pub fn collect_binary_info(program: &str) -> Option<BinaryInfo> {
    let path = std::path::PathBuf::from(program);
    let metadata = std::fs::metadata(&path).ok()?;
    let modified = metadata.modified().ok()?;
    if let Some(cache) = BINARY_INFO_CACHE.lock().unwrap().get(program) {
        if cache.0 == modified && cache.1 == metadata.len() {
            return Some(cache.2.clone());
        }
    }

    let name = path
        .file_name()
        .map(|n| n.to_string_lossy().into_owned())
        .unwrap_or_else(|| program.to_string());
    let path_str = path.display().to_string();

    let compile_time = modified
        .duration_since(std::time::UNIX_EPOCH)
        .ok()
        .map(|d| {
            use chrono::TimeZone;
            let dt = chrono::Local.timestamp_opt(d.as_secs() as i64, 0).unwrap();
            dt.format("%Y-%m-%d %H:%M:%S").to_string()
        })
        .unwrap_or_else(|| "unknown".to_string());

    let bytes = std::fs::read(&path).ok()?;
    let (hash, hash_short) = match scan_family_identity(&bytes) {
        Some(stamp) => (stamp.clone(), stamp),
        None => {
            let hash = compute_file_hash(&bytes);
            let trimmed = hash.trim_end_matches('=');
            let short = if trimmed.len() > 6 {
                trimmed[trimmed.len() - 6..].to_string()
            } else {
                trimmed.to_string()
            };
            (hash, short)
        }
    };

    let info = BinaryInfo {
        name,
        path: path_str,
        compile_time,
        hash,
        hash_short,
    };
    BINARY_INFO_CACHE.lock().unwrap().insert(
        program.to_string(),
        (modified, metadata.len(), info.clone()),
    );
    Some(info)
}

/// The `<branch>` charset of a family identity line.
fn is_branch_byte(b: u8) -> bool {
    b.is_ascii_alphanumeric() || matches!(b, b'.' | b'_' | b'/' | b'-')
}

fn is_hex_byte(b: u8) -> bool {
    b.is_ascii_digit() || (b'a'..=b'f').contains(&b)
}

/// Extract the single family build identity from the binary's bytes, if
/// the binary states exactly one: scan printable-ASCII runs for
/// `<x.y.z> <branch>:<hash7..40>` and require every hit to agree. Zero
/// hits → `None` (caller falls back); two disagreeing hits → `None`
/// (ambiguous, never guess).
fn scan_family_identity(bytes: &[u8]) -> Option<String> {
    let mut identities: Vec<(String, String)> = Vec::new();
    let mut run: Vec<u8> = Vec::with_capacity(128);
    let flush = |run: &mut Vec<u8>, identities: &mut Vec<(String, String)>| {
        if !run.is_empty() {
            scan_ascii_run(run, identities);
            run.clear();
        }
    };
    for &b in bytes {
        if b.is_ascii_graphic() || b == b' ' {
            run.push(b);
        } else {
            flush(&mut run, &mut identities);
        }
    }
    flush(&mut run, &mut identities);
    if identities.len() != 1 {
        return None;
    }
    let (branch, hash) = &identities[0];
    Some(format!("{branch}:{hash}"))
}

/// One printable-ASCII run: find `<base> <branch>:<hash>` occurrences.
fn scan_ascii_run(run: &[u8], identities: &mut Vec<(String, String)>) {
    let n = run.len();
    let mut i = 0;
    while i < n {
        // Candidate base start: a digit not preceded by digit/dot (avoid
        // re-matching inside a longer version literal).
        if !run[i].is_ascii_digit()
            || (i > 0 && (run[i - 1].is_ascii_digit() || run[i - 1] == b'.'))
        {
            i += 1;
            continue;
        }
        let Some(after_base) = match_version_base(run, i) else {
            i += 1;
            continue;
        };
        if after_base >= n || run[after_base] != b' ' {
            i += 1;
            continue;
        }
        let branch_start = after_base + 1;
        let mut j = branch_start;
        while j < n && is_branch_byte(run[j]) {
            j += 1;
        }
        if j == branch_start || j + 1 >= n {
            i += 1;
            continue;
        }
        // One or two colons (the family narrowed `::` → `:` on 2026-10-11;
        // transition-window binaries still speak either).
        let mut k = j;
        if run[k] == b':' {
            k += 1;
        }
        if k < n && run[k] == b':' {
            k += 1;
        }
        if k == j || k >= n || !is_hex_byte(run[k]) {
            i += 1;
            continue;
        }
        let hash_start = k;
        let mut e = k;
        while e < n && is_hex_byte(run[e]) && e - hash_start < 40 {
            e += 1;
        }
        let hash_len = e - hash_start;
        if !(7..=40).contains(&hash_len) {
            i += 1;
            continue;
        }
        let branch = String::from_utf8_lossy(&run[branch_start..j]).into_owned();
        let hash7 = String::from_utf8_lossy(&run[hash_start..hash_start + 7]).into_owned();
        let identity = (branch, hash7);
        if !identities.contains(&identity) {
            identities.push(identity);
        }
        i = e;
    }
}

/// `x.y.z` at `start` → index just past it, else `None`.
fn match_version_base(run: &[u8], start: usize) -> Option<usize> {
    let mut i = start;
    let mut dots = 0;
    while i < run.len() && run[i].is_ascii_digit() {
        i += 1;
        if i < run.len() && run[i] == b'.' && dots < 2 {
            dots += 1;
            i += 1;
        }
    }
    if dots == 2 && i > start + 1 && (i >= run.len() || run[i] != b'.') {
        Some(i)
    } else {
        None
    }
}

fn compute_file_hash(bytes: &[u8]) -> String {
    use sha2::Digest;
    let mut hasher = sha2::Sha256::new();
    hasher.update(bytes);
    base32_encode(&hasher.finalize())
}

const BASE32_ALPHABET: &[u8] = b"ABCDEFGHIJKLMNOPQRSTUVWXYZ234567";

fn base32_encode(bytes: &[u8]) -> String {
    let mut out = String::with_capacity((bytes.len() * 8).div_ceil(5));
    let mut buffer: u32 = 0;
    let mut bits: u32 = 0;
    for &b in bytes {
        buffer = (buffer << 8) | b as u32;
        bits += 8;
        while bits >= 5 {
            bits -= 5;
            out.push(BASE32_ALPHABET[((buffer >> bits) & 31) as usize] as char);
        }
    }
    if bits > 0 {
        out.push(BASE32_ALPHABET[((buffer << (5 - bits)) & 31) as usize] as char);
    }
    while out.len() % 8 != 0 {
        out.push('=');
    }
    out
}

/// Re-resolve the supervised binaries' identities for a render. Cheap
/// (mtime+size memo per program) and honest across deploys: the watched
/// binary can be swapped under a running malkuth, so every landing-page
/// render states the identity of the file that is there NOW.
async fn collect_binaries(programs: &[String]) -> Vec<BinaryInfo> {
    if programs.is_empty() {
        return vec![];
    }
    let programs = programs.to_vec();
    tokio::task::spawn_blocking(move || {
        programs
            .iter()
            .filter_map(|p| collect_binary_info(p))
            .collect()
    })
    .await
    .unwrap_or_default()
}

fn detect_install_method() -> &'static str {
    if std::env::var_os("CARGO_MANIFEST_DIR").is_some() {
        static C: &str = "cargo";
        return C;
    }
    if let Ok(exe) = std::env::current_exe() {
        if let Ok(p) = exe.canonicalize() {
            let s = p.to_string_lossy();
            if s.contains("/node_modules/") || s.contains("\\node_modules\\") {
                static N: &str = "npm";
                return N;
            }
        }
    }
    if let Ok(path) = std::env::var("PATH") {
        for dir in path.split(':') {
            let d = dir.trim();
            if d.ends_with("/nvm")
                || d.ends_with("\\nvm")
                || std::path::Path::new(d).join("versions").exists()
            {
                static N: &str = "nvm";
                return N;
            }
        }
    }
    static X: &str = "";
    X
}

/// Build an axum Router that serves the Malkuth info page on every request.
/// When `serve_backend` is set, the handler acts as an HTTP reverse proxy:
/// - First visit (no `__malkuth_nonce` cookie) → landing page reflecting the
///   probed backend state (Up → redirect countdown, NotReady → starting-up
///   notice, Down → offline notice)
/// - Repeat visit (nonce set) & backend Up → forward the request; the
///   backend response passes through untouched (status, headers, body).
///   WebSocket (or any HTTP/1.1 upgrade) handshakes are tunneled over raw
///   TCP instead of going through reqwest, which cannot upgrade.
/// - Backend unreachable / unavailable (transport failure or 502/503/504)
///   → render the info/landing page
#[allow(clippy::too_many_arguments)]
pub fn info_router(
    version: impl Into<String>,
    status: InfoStatus,
    watch_paths: Vec<String>,
    proxy_endpoint: Option<String>,
    // Supervised PROGRAM paths whose identity the landing page shows.
    // Collected per render (see [`collect_binaries`]) — not a startup
    // snapshot, so a deploy that swaps a watched binary (malkuth itself
    // keeps running) is reflected on the next landing-page view.
    binary_programs: Vec<String>,
    serve_backend: Option<String>,
    serve_hosts: Vec<String>,
    build_progress: std::sync::Arc<std::sync::Mutex<Option<String>>>,
    build_log: std::sync::Arc<std::sync::Mutex<Vec<String>>>,
    runtime_log: std::sync::Arc<std::sync::Mutex<Vec<String>>>,
) -> Router<()> {
    let backend_state = std::sync::Arc::new(tokio::sync::RwLock::new(BackendState::Unknown));
    let epoch = std::sync::Arc::new(tokio::sync::RwLock::new("1".to_string()));

    if let Some(ref backend_url) = serve_backend {
        let state_arc = backend_state.clone();
        let epoch_arc = epoch.clone();
        let url = backend_url.clone();
        // Probe once synchronously so the very first landing-page request
        // already knows whether the backend is reachable (no Unknown window)
        // and which build the backend currently serves.
        let initial = probe_backend(&url);
        if let Some(e) = probe_backend_epoch(&url) {
            if let Ok(mut w) = epoch_arc.try_write() {
                *w = e;
            }
        }
        if let Ok(mut w) = state_arc.try_write() {
            *w = initial;
        }
        tokio::spawn(async move {
            loop {
                // Blocking probe on the blocking thread pool so slow backends
                // never stall an async worker.
                let probe_url = url.clone();
                let (state, token) = tokio::task::spawn_blocking(move || {
                    (probe_backend(&probe_url), probe_backend_epoch(&probe_url))
                })
                .await
                .unwrap_or((BackendState::Down, None));
                let mut w = state_arc.write().await;
                *w = state;
                drop(w);
                // A failed epoch probe keeps the previous token: a backend
                // that briefly fails to answer `/` has not necessarily
                // rolled back to an older build.
                if let Some(e) = token {
                    let mut ew = epoch_arc.write().await;
                    *ew = e;
                }
                tokio::time::sleep(Duration::from_secs(5)).await;
            }
        });
    }

    let state = InfoState {
        version: version.into(),
        status,
        watch_paths,
        proxy_endpoint,
        binary_programs,
        serve_backend,
        serve_hosts,
        build_progress,
        build_log,
        runtime_log,
        backend_state,
        epoch,
    };
    // All methods: with --serve the landing page doubles as a full reverse
    // proxy (JSON-RPC POSTs, PUT/PATCH/DELETE, WS upgrades…). GET-only routing
    // answered 405 for every non-GET request — e.g. the webui login (POST
    // /api/rpc) could never reach the backend through the front door.
    Router::new()
        .route("/", any(info_page))
        .fallback(any(info_page))
        .with_state(state)
}

#[derive(Clone)]
struct InfoState {
    version: String,
    status: InfoStatus,
    watch_paths: Vec<String>,
    proxy_endpoint: Option<String>,
    binary_programs: Vec<String>,
    serve_backend: Option<String>,
    serve_hosts: Vec<String>,
    build_progress: std::sync::Arc<std::sync::Mutex<Option<String>>>,
    build_log: std::sync::Arc<std::sync::Mutex<Vec<String>>>,
    runtime_log: std::sync::Arc<std::sync::Mutex<Vec<String>>>,
    backend_state: std::sync::Arc<tokio::sync::RwLock<BackendState>>,
    /// Build token currently served by the `--serve` backend (see
    /// [`probe_backend_epoch`]). Clients whose `__malkuth_nonce` cookie
    /// matches it have already seen the current build.
    epoch: std::sync::Arc<tokio::sync::RwLock<String>>,
}

/// Probe the `--serve` backend's reachability with `GET /readyz`.
///
/// Verdict precedence:
/// - TCP connect fails / no valid HTTP status line → [`BackendState::Down`]
/// - HTTP 503 (k8s-style readiness failure, what malkuth-instrumented
///   services report while starting or draining) → [`BackendState::NotReady`]
/// - any other HTTP status → [`BackendState::Up`]. API-only backends
///   legitimately answer 404 on unknown paths, which must not read as
///   "unreachable".
///
/// `url` may carry a scheme and/or path (both are stripped for the probe).
/// Plain HTTP only — a TLS backend cannot answer this probe.
fn probe_backend(url: &str) -> BackendState {
    let host_port = url
        .trim_start_matches("http://")
        .trim_start_matches("https://")
        .split(['/', '?'])
        .next()
        .unwrap_or("");
    let addr: std::net::SocketAddr = match host_port.parse().ok() {
        Some(a) => a,
        None => return BackendState::Down,
    };
    let mut stream = match std::net::TcpStream::connect_timeout(&addr, Duration::from_millis(500)) {
        Ok(s) => s,
        Err(_) => return BackendState::Down,
    };
    use std::io::Write;
    let req = format!("GET /readyz HTTP/1.0\r\nHost: {host_port}\r\nConnection: close\r\n\r\n");
    if stream.write_all(req.as_bytes()).is_err() {
        return BackendState::Down;
    }
    match read_http_status_code(&mut stream, Duration::from_millis(1500)) {
        Some(503) => BackendState::NotReady,
        Some(_) => BackendState::Up,
        None => BackendState::Down,
    }
}

/// Lowercase hex encoder (no external dependency, mirrors `base64_encode`'s
/// self-contained style).
fn hex_encode(bytes: &[u8]) -> String {
    const HEX: &[u8; 16] = b"0123456789abcdef";
    let mut out = String::with_capacity(bytes.len() * 2);
    for b in bytes {
        out.push(HEX[(b >> 4) as usize] as char);
        out.push(HEX[(b & 0x0f) as usize] as char);
    }
    out
}

/// Probe the `--serve` backend's document root (`GET /`) and derive a build
/// token: the first 12 hex chars of the SHA-256 over the response body,
/// taken only when the backend answers 200 with an HTML document.
///
/// The token lets the serve-mode front door tell "this client has already
/// seen the current build" apart from "a new build landed since the
/// client's last visit": every rebuild changes the served `index.html`
/// (hashed asset references) and therefore the token, so each client is
/// shown the landing interstitial exactly once per build — including
/// clients that first visit long after the rebuild.
///
/// Non-HTML answers (API-only backends), non-200 statuses, empty bodies,
/// and transport failures yield `None`; the caller then keeps the previous
/// token, degrading to the plain first-visit behaviour for backends that
/// serve no HTML document.
fn probe_backend_epoch(url: &str) -> Option<String> {
    use sha2::Digest;
    let host_port = url
        .trim_start_matches("http://")
        .trim_start_matches("https://")
        .split(['/', '?'])
        .next()
        .unwrap_or("");
    let addr: std::net::SocketAddr = host_port.parse().ok()?;
    let mut stream = match std::net::TcpStream::connect_timeout(&addr, Duration::from_millis(500)) {
        Ok(s) => s,
        Err(_) => return None,
    };
    use std::io::{Read, Write};
    // HTTP/1.0 + Connection: close keeps the exchange EOF-delimited without
    // a chunked/keep-alive parser; no Accept-Encoding means an identity
    // body, so the hash is over the exact bytes a browser would receive.
    let req = format!(
        "GET / HTTP/1.0\r\nHost: {host_port}\r\nAccept: text/html\r\nConnection: close\r\n\r\n"
    );
    stream.write_all(req.as_bytes()).ok()?;
    let _ = stream.set_read_timeout(Some(Duration::from_millis(1500)));
    let mut buf = Vec::with_capacity(8 * 1024);
    // A read timeout surfaces as an error after partial data; bytes read so
    // far are retained in `buf`, which is still a stable hash input.
    if stream.read_to_end(&mut buf).is_err() {
        // Body incomplete within the read budget (slow/trickling backend).
        // Hashing the partial bytes would risk a token that flips between
        // probe cycles whenever the delivery time jitters around the
        // budget, re-showing the landing interstitial on every flip.
        // Keep the previous token instead: one missed update costs at
        // most a late landing cycle, never a flap.
        tracing::warn!(
            bytes = buf.len(),
            "epoch probe body read incomplete; keeping previous build token"
        );
        return None;
    }
    if buf.len() > 512 * 1024 {
        buf.truncate(512 * 1024);
    }
    let head_end = buf.windows(4).position(|w| w == b"\r\n\r\n")?;
    let head = std::str::from_utf8(&buf[..head_end]).ok()?;
    let mut lines = head.split("\r\n");
    let status_is_200 = lines
        .next()?
        .split_whitespace()
        .nth(1)
        .is_some_and(|code| code == "200");
    if !status_is_200 {
        return None;
    }
    let is_html = lines.any(|l| {
        l.split_once(':').is_some_and(|(name, value)| {
            name.trim().eq_ignore_ascii_case("content-type")
                && value.to_ascii_lowercase().contains("text/html")
        })
    });
    if !is_html {
        return None;
    }
    let body = &buf[head_end + 4..];
    if body.is_empty() {
        return None;
    }
    let hash = sha2::Sha256::digest(body);
    Some(hex_encode(&hash)[..12].to_string())
}

/// Read one HTTP status line (until `\n`, capped at 64 bytes) with a total
/// deadline, tolerating short reads and slow first bytes. Returns the
/// numeric status code, or `None` when no well-formed status line arrives.
fn read_http_status_code(stream: &mut std::net::TcpStream, budget: Duration) -> Option<u16> {
    use std::io::Read;
    let deadline = std::time::Instant::now() + budget;
    let mut line = Vec::with_capacity(64);
    let mut byte = [0u8; 1];
    loop {
        if line.len() >= 64 {
            break;
        }
        let now = std::time::Instant::now();
        if now >= deadline {
            break;
        }
        let _ = stream.set_read_timeout(Some(deadline - now));
        match stream.read(&mut byte) {
            Ok(0) => break,
            Ok(_) => {
                if byte[0] == b'\n' {
                    break;
                }
                line.push(byte[0]);
            }
            Err(_) => break,
        }
    }
    let head = std::str::from_utf8(&line).unwrap_or("");
    // "HTTP/1.1 200 OK" → strip version token, then take the code.
    let after = head
        .strip_prefix("HTTP/1.")
        .or_else(|| head.strip_prefix("HTTP/2"))
        .or_else(|| head.strip_prefix("HTTP/3"))?;
    let code_part = after
        .trim_start_matches(|c: char| c.is_ascii_digit() || c == '.')
        .trim_start();
    code_part.split_whitespace().next()?.parse().ok()
}

async fn build_spa_init(state: &InfoState, lang: &str, allowed: bool) -> serde_json::Value {
    let i18n = get_i18n(lang);
    let binaries = collect_binaries(&state.binary_programs).await;
    let backend = *state.backend_state.read().await;
    let (spa_state, status_key) = page_state(state, backend);
    let message = i18n.get(status_key).map_or("", |v| v.as_str());
    let dir = if lang == "ar" { "rtl" } else { "ltr" };
    serde_json::json!({
        // ── v1 keys (kept verbatim; the SPA reads all of them) ────────────
        "state": spa_state,
        "message": message,
        "binaries": binaries,
        "watch_paths": state.watch_paths,
        "proxy_endpoint": state.proxy_endpoint,
        "version": state.version,
        "epoch": state.epoch.read().await.clone(),
        "logo_base64": base64_encode(LOGO_BYTES),
        // Whether a reload of this origin can actually be proxied into the
        // backend (see `serve_allowed`). The landing page uses it to skip its
        // poll-driven ready-reload on doors that can only re-render
        // themselves, where reloading is a no-op loop.
        "serve": state.serve_backend.is_some() && allowed,
        // ── v2 keys: locale + the full localized catalogue ────────────────
        // The SPA used to carry its own en/zh copy table; it now renders the
        // same 11-locale catalogue the server-side pages use, so a new key
        // (or a new language) is added in exactly one place: all.json.
        "lang": lang,
        "dir": dir,
        "title": i18n.get("title").cloned().unwrap_or_default(),
        "i18n": i18n,
        "timing": { "countdown_s": 3, "probe_interval_ms": 2000 },
        "nonce": { "cookie": "__malkuth_nonce", "max_age_s": 604800, "path": "/" },
    })
}

async fn serve_spa(state: &InfoState, lang: &str, allowed: bool) -> Response {
    let init_data = build_spa_init(state, lang, allowed).await;
    let init_script = format!(
        "<script>window.__MALKUTH_INIT__ = {};</script>",
        init_json_script(&init_data)
    );
    let result = LANDING_PAGE_HTML.replacen("</head>", &format!("{init_script}</head>"), 1);
    Html(result).into_response()
}

/// Whether a proxied backend status means "backend unavailable" and should
/// be masked with the landing page. Only gateway-style statuses qualify:
/// 502/503/504 signal a backend that is down or draining — mirroring
/// [`probe_backend`], where 503 is the single not-ready verdict and every
/// other HTTP status counts as up. All other statuses (404, 3xx, 500, ...)
/// are live backend answers and pass through to the browser untouched.
fn should_fall_back(status: u16) -> bool {
    matches!(status, 502..=504)
}

/// Whether the request asks for an HTTP/1.1 protocol upgrade. Both a
/// `Connection` header containing the `upgrade` token (token-list match,
/// case-insensitive) and an `Upgrade` header must be present. The `Upgrade`
/// value is intentionally not restricted to `websocket` — generic upgrade
/// semantics are tunneled the same way.
fn is_upgrade_request(headers: &header::HeaderMap) -> bool {
    headers.contains_key(header::UPGRADE)
        && headers
            .get_all(header::CONNECTION)
            .iter()
            .filter_map(|v| v.to_str().ok())
            .any(|v| {
                v.split(',')
                    .any(|token| token.trim().eq_ignore_ascii_case("upgrade"))
            })
}

/// Audit malkuth-R1-6: cap the proxied request body at 1 MiB — a client can
/// no longer OOM the supervisor with a giant request body. Shared by both
/// proxy paths ([`proxy_upgrade`] and [`proxy_to_backend_parts`]).
///
/// The cap is runtime-configurable because it is also a front-door BODY LIMIT:
/// a face proxied through here silently truncates at 1 MiB whatever the backend
/// allows (the demo face's 200 + landing HTML with the stream cut is that
/// failure), so the operator must be able to align the two. Default unchanged —
/// an instance that sets nothing keeps the R1-6 posture exactly.
///
/// Raising it costs MEMORY, not just bytes: the proxy buffers the whole request
/// (`axum::body::to_bytes`) before re-writing it, so resident cost is
/// `cap × concurrent proxied requests`, inside this process. Match the largest
/// backend path you actually serve:
///
/// * chest's JSON-RPC /api face — 10 MiB request limit, so ≈12 MiB here;
/// * chest document uploads — 50 MiB cap, so ≈52 MiB here;
/// * chest model uploads — 512 MiB cap, so ≈514 MiB here, which at concurrency
///   is a memory decision first and a limit decision second.
pub const MAX_PROXY_BODY_BYTES_ENV: &str = "MALKUTH_PROXY_MAX_BODY_BYTES";

/// The unchanged R1-6 default (1 MiB).
pub const DEFAULT_MAX_PROXY_BODY_BYTES: usize = 1024 * 1024;

/// The largest cap the doc above actually recommends (chest's 512 MiB model
/// uploads, plus slack). A value above this is still HONOURED — refusing an
/// operator's explicit number would be blunter than the problem — but it is
/// warned about, because `usize` accepts values that quietly repeal R1-6's
/// protection (a cap of `usize::MAX` is "no cap", at 1 byte of RAM per byte of
/// body).
pub const LARGEST_DOCUMENTED_CLASS_BYTES: usize = 514 * 1024 * 1024;

/// Resolved once per process: the env is deployment shape, not per-request
/// input, and a mid-flight change would make the cap depend on timing.
static MAX_PROXY_BODY_BYTES: std::sync::OnceLock<usize> = std::sync::OnceLock::new();

/// Parse the override. Pure, so the rules are testable without mutating the
/// process environment (which is process-global and racy under a threaded test
/// runner).
///
/// `0` is refused: it would reject every proxied request with a body, turning a
/// typo into a dead face. Unparsable values are refused the same way. Both fall
/// back to the default LOUDLY — a silent fallback would look exactly like a
/// working deployment until an upload dies at 1 MiB.
pub(crate) fn resolve_max_proxy_body_bytes(raw: Option<&str>) -> usize {
    let Some(raw) = raw else {
        return DEFAULT_MAX_PROXY_BODY_BYTES;
    };
    match raw.trim().parse::<usize>() {
        Ok(bytes) if bytes > 0 => {
            if bytes > LARGEST_DOCUMENTED_CLASS_BYTES {
                tracing::warn!(
                    env = MAX_PROXY_BODY_BYTES_ENV,
                    cap_bytes = bytes,
                    documented_max = LARGEST_DOCUMENTED_CLASS_BYTES,
                    "proxy body cap is above the largest documented backend class — honoured, but \
                     the buffer cost is cap × concurrent proxied requests of resident memory, and \
                     a very large value effectively repeals the R1-6 anti-OOM protection"
                );
            }
            bytes
        }
        Ok(_) => {
            tracing::warn!(
                env = MAX_PROXY_BODY_BYTES_ENV,
                value = %raw,
                "proxy body cap of 0 is refused (it would reject every request with a body) — \
                 keeping the 1 MiB default"
            );
            DEFAULT_MAX_PROXY_BODY_BYTES
        }
        Err(_) => {
            tracing::warn!(
                env = MAX_PROXY_BODY_BYTES_ENV,
                value = %raw,
                "proxy body cap is not a byte count (expected e.g. 12582912) — keeping the 1 MiB \
                 default"
            );
            DEFAULT_MAX_PROXY_BODY_BYTES
        }
    }
}

/// The effective cap for this process.
pub(crate) fn max_proxy_body_bytes() -> usize {
    *MAX_PROXY_BODY_BYTES.get_or_init(|| {
        resolve_max_proxy_body_bytes(std::env::var(MAX_PROXY_BODY_BYTES_ENV).ok().as_deref())
    })
}

/// Buffer a proxied request body under the cap.
///
/// Both proxy paths call THIS, so the cap cannot be raised on one path and left
/// behind on the other — the two used to share a constant, and a shared
/// function is the version of that which a future edit cannot break.
async fn read_proxied_body(body: axum::body::Body) -> Result<axum::body::Bytes, ()> {
    read_proxied_body_with(body, max_proxy_body_bytes()).await
}

/// The cap application, with the cap passed in — the seam that makes "the knob
/// is actually wired to the reader" testable without setting process env (which
/// is global and racy under a threaded test runner) and without allocating a
/// real cap-sized body (the doc above recommends up to ~514 MiB; a test must
/// not need that much RAM to prove an 8-byte cap is enforced).
async fn read_proxied_body_with(
    body: axum::body::Body,
    cap: usize,
) -> Result<axum::body::Bytes, ()> {
    axum::body::to_bytes(body, cap).await.map_err(|e| {
        // A refusal here is NOT a transport failure, but the caller's fallback
        // makes it look like one: the request lands on the landing page as a
        // `200`, and nothing downstream ever learns why. Name the cap and the
        // knob, or the operator's only symptom is "big uploads die silently" —
        // which is the exact failure this setting exists to end.
        tracing::warn!(
            env = MAX_PROXY_BODY_BYTES_ENV,
            cap_bytes = cap,
            error = %e,
            "refused a proxied request body that did not fit the cap — the client sees the \
             landing page, so this log is the only place the reason appears; raise {} to accept \
             larger bodies",
            MAX_PROXY_BODY_BYTES_ENV
        )
    })
}

/// Extract the bare `host:port` of the `--serve` backend URL, dropping any
/// scheme and path — plain HTTP only, mirroring [`probe_backend`].
fn backend_host_port(backend: &str) -> Option<&str> {
    let host_port = backend
        .trim_start_matches("http://")
        .trim_start_matches("https://")
        .split(['/', '?'])
        .next()?;
    (!host_port.is_empty()).then_some(host_port)
}

/// Build the `__malkuth_nonce` Set-Cookie value that marks a client as
/// having seen the `epoch` build. Long-lived on purpose: the landing
/// interstitial is driven by build-token mismatch (every recompile
/// changes the token), not by cookie expiry, so a client is never dumped
/// back onto the landing page mid-session for a purely timed reason.
fn nonce_cookie(epoch: &str) -> header::HeaderValue {
    let raw = format!("__malkuth_nonce={epoch}; max-age=604800; path=/");
    header::HeaderValue::from_str(&raw).unwrap_or_else(|_| {
        header::HeaderValue::from_static("__malkuth_nonce=1; max-age=604800; path=/")
    })
}

/// Reverse-proxy an HTTP/1.1 upgrade handshake (WebSocket or other) to the
/// `--serve` backend over a raw TCP tunnel. reqwest strips hop-by-hop
/// headers and has no upgrade semantics, so the handshake is written to the
/// backend verbatim; on a 101 the backend's response headers pass through
/// untouched and both byte streams are spliced together. Non-101 answers
/// (e.g. 404/426) pass through with their full body, with the same
/// semantics as [`proxy_to_backend`]: only transport failures and
/// 502/503/504 fall back to the landing page.
async fn proxy_upgrade(req: Request, backend: &str, epoch: &str) -> Result<Response, ()> {
    let (mut parts, body) = req.into_parts();

    // hyper stores the pending upgrade in the request extensions. Every
    // axum http1 request carrying an upgrade token gets one; its absence
    // means the server cannot upgrade this connection, so fall back to the
    // plain proxy rather than answering 500.
    let Some(on_upgrade) = parts.extensions.remove::<OnUpgrade>() else {
        return proxy_to_backend_parts(parts, body, backend, epoch).await;
    };

    let Some(host_port) = backend_host_port(backend) else {
        return Err(());
    };
    let path_and_query = parts.uri.path_and_query().map_or("/", |pq| pq.as_str());

    // WS requests normally carry no body; if one does, read it fully so the
    // forwarded request is complete before the upgrade exchange starts.
    let body_bytes = read_proxied_body(body).await?;

    // Dial the backend (plain HTTP only, like the readiness probe). Any
    // connect failure is a transport error → landing page.
    let mut stream =
        tokio::time::timeout(Duration::from_millis(2000), TcpStream::connect(host_port))
            .await
            .map_err(|_| ())?
            .map_err(|_| ())?;

    // Forward the request verbatim over raw HTTP/1.1: hop-by-hop headers
    // (Host/Connection/Transfer-Encoding) are dropped, Host is reset to the
    // backend, `Connection: Upgrade` is re-added so the backend treats this
    // as an upgrade request, and everything else — Upgrade,
    // Sec-WebSocket-*, Cookie, ... — goes through byte-for-byte. The
    // malkuth nonce cookie is stripped for symmetry with
    // `proxy_to_backend`.
    let mut wire: Vec<u8> = Vec::with_capacity(512);
    wire.extend_from_slice(parts.method.as_str().as_bytes());
    wire.push(b' ');
    wire.extend_from_slice(path_and_query.as_bytes());
    wire.extend_from_slice(b" HTTP/1.1\r\nHost: ");
    wire.extend_from_slice(host_port.as_bytes());
    wire.extend_from_slice(b"\r\n");
    for (name, value) in parts.headers.iter() {
        let lower = name.as_str().to_lowercase();
        // Content-Length is dropped here and re-added below from the fully
        // buffered body — keeping the client's copy would send a duplicate
        // framing header (rejected as 400 by hyper/axum backends).
        if lower == "host"
            || lower == "connection"
            || lower == "transfer-encoding"
            || lower == "content-length"
        {
            continue;
        }
        if lower == "cookie" {
            let Some(cookies) = value.to_str().ok() else {
                continue;
            };
            let cleaned: Vec<&str> = cookies
                .split(';')
                .filter(|c| !c.trim().starts_with("__malkuth_nonce="))
                .collect();
            if cleaned.is_empty() {
                continue;
            }
            wire.extend_from_slice(b"Cookie: ");
            wire.extend_from_slice(cleaned.join(";").as_bytes());
            wire.extend_from_slice(b"\r\n");
            continue;
        }
        wire.extend_from_slice(name.as_str().as_bytes());
        wire.extend_from_slice(b": ");
        wire.extend_from_slice(value.as_bytes());
        wire.extend_from_slice(b"\r\n");
    }
    if !body_bytes.is_empty() {
        wire.extend_from_slice(b"Content-Length: ");
        wire.extend_from_slice(body_bytes.len().to_string().as_bytes());
        wire.extend_from_slice(b"\r\n");
    }
    wire.extend_from_slice(b"Connection: Upgrade\r\n\r\n");
    // Bounded write so a backend that accepts but never reads cannot stall
    // the handler (the kernel buffer absorbs tiny requests, so this only
    // trips on genuinely wedged peers).
    let wire_flush = async {
        stream.write_all(&wire).await?;
        if !body_bytes.is_empty() {
            stream.write_all(&body_bytes).await?;
        }
        stream.flush().await
    };
    tokio::time::timeout(Duration::from_secs(5), wire_flush)
        .await
        .map_err(|_| ())?
        .map_err(|_| ())?;

    // Read the response head (status line + headers, byte-capped so a
    // hostile backend cannot buffer without bound) and any bytes the
    // backend already sent past the head — those belong to the upgraded
    // stream and must not be lost.
    let (head, leftover) =
        tokio::time::timeout(Duration::from_secs(10), read_response_head(&mut stream))
            .await
            .map_err(|_| ())?
            .map_err(|_| ())?;
    let (status, resp_headers) = parse_response_head(&head).ok_or(())?;

    if status == 101 {
        // Build the 101 for the browser with the backend's headers verbatim
        // (Upgrade / Connection / Sec-WebSocket-Accept / Sec-WebSocket-*).
        let mut response = Response::new(axum::body::Body::empty());
        *response.status_mut() = StatusCode::SWITCHING_PROTOCOLS;
        for (name, value) in resp_headers {
            if let (Ok(n), Ok(v)) = (
                header::HeaderName::from_bytes(name.as_bytes()),
                header::HeaderValue::from_bytes(&value),
            ) {
                response.headers_mut().append(n, v);
            }
        }
        // No nonce Set-Cookie on the 101: after an upgrade there is no
        // further HTTP exchange to gate, browsers ignore Set-Cookie on
        // upgrade responses anyway, and the JS-set nonce cookie keeps
        // working. Keep the upgrade payload minimal.

        // The `OnUpgrade` future only resolves once the 101 has been
        // flushed to the browser, so the handler must return first and the
        // tunnel runs detached. `copy_bidirectional` shuts down the
        // opposing writer on EOF — exactly the half-close semantics the
        // WebSocket close handshake relies on — and ends the tunnel when
        // either side disconnects or errors, dropping both streams.
        tokio::spawn(async move {
            let upgraded = match on_upgrade.await {
                Ok(stream) => stream,
                Err(_) => return,
            };
            let mut client_io = TokioIo::new(upgraded);
            if !leftover.is_empty() {
                // Bytes the backend sent right after the head (e.g. a
                // server-initiated frame) reach the browser first.
                let mut rest = std::io::Cursor::new(leftover);
                if tokio::io::copy(&mut rest, &mut client_io).await.is_err() {
                    return;
                }
            }
            let _ = tokio::io::copy_bidirectional(&mut client_io, &mut stream).await;
        });
        return Ok(response);
    }

    // Non-101: the backend answered in HTTP. Forward the whole response
    // untouched — 404/426/400 and friends are live backend answers and must
    // not be masked (same rule as `proxy_to_backend`); only 502/503/504
    // fall back to the landing page.
    if should_fall_back(status) {
        return Err(());
    }

    // Read the full body: framed exactly via Content-Length / chunked
    // transfer, otherwise read until EOF (a backend that neither frames nor
    // closes within the budget is a transport failure).
    let deadline = tokio::time::Instant::now() + Duration::from_secs(30);
    let resp_body = read_response_body(&mut stream, &resp_headers, leftover, deadline).await?;

    // Mirror `proxy_to_backend`'s response assembly: nonce cookie appended
    // alongside (never replacing) backend Set-Cookie headers; hop-by-hop
    // headers stripped since the body was already decoded.
    let mut backend_cookies: Vec<Vec<u8>> = Vec::new();
    let resp_headers: Vec<(String, Vec<u8>)> = resp_headers
        .into_iter()
        .filter(|(name, value)| {
            let lower = name.to_lowercase();
            if lower == "set-cookie" {
                backend_cookies.push(value.clone());
                return false;
            }
            lower != "transfer-encoding" && lower != "connection"
        })
        .collect();

    let status_code = StatusCode::from_u16(status).unwrap_or(StatusCode::OK);
    let mut response = Response::new(axum::body::Body::from(resp_body));
    *response.status_mut() = status_code;
    response
        .headers_mut()
        .append(header::SET_COOKIE, nonce_cookie(epoch));
    for value in backend_cookies {
        if let Ok(v) = header::HeaderValue::from_bytes(&value) {
            response.headers_mut().append(header::SET_COOKIE, v);
        }
    }
    for (name, value) in resp_headers {
        if let (Ok(n), Ok(v)) = (
            header::HeaderName::from_bytes(name.as_bytes()),
            header::HeaderValue::from_bytes(&value),
        ) {
            response.headers_mut().insert(n, v);
        }
    }
    Ok(response)
}

/// Read a backend response head (status line + headers up to `\r\n\r\n`),
/// capped at 64 KiB. Returns the head and any bytes read past it (the head
/// of the upgraded stream, for a 101).
async fn read_response_head(stream: &mut TcpStream) -> Result<(Vec<u8>, Vec<u8>), ()> {
    const CAP: usize = 64 * 1024;
    let mut buf: Vec<u8> = Vec::with_capacity(1024);
    let mut chunk = [0u8; 4096];
    loop {
        if buf.len() >= CAP {
            return Err(());
        }
        let n = stream.read(&mut chunk).await.map_err(|_| ())?;
        if n == 0 {
            // Backend hung up mid-headers: nothing to forward.
            return Err(());
        }
        buf.extend_from_slice(&chunk[..n]);
        if let Some(end) = buf.windows(4).position(|w| w == b"\r\n\r\n").map(|p| p + 4) {
            let leftover = buf.split_off(end);
            return Ok((buf, leftover));
        }
    }
}

/// A parsed response header entry: lowercased name + raw value bytes.
type ParsedHeader = (String, Vec<u8>);

/// Parse a response head into `(status, headers)` with lowercased names.
fn parse_response_head(head: &[u8]) -> Option<(u16, Vec<ParsedHeader>)> {
    let text = std::str::from_utf8(head).ok()?;
    let mut lines = text.split("\r\n");
    let status_line = lines.next()?;
    let status = status_line.split_whitespace().nth(1)?.parse::<u16>().ok()?;
    let mut headers = Vec::new();
    for line in lines {
        let Some((name, value)) = line.split_once(':') else {
            continue;
        };
        headers.push((
            name.trim().to_ascii_lowercase(),
            value.trim().as_bytes().to_vec(),
        ));
    }
    Some((status, headers))
}

/// Read the body of a non-101 backend response. `body` already holds any
/// bytes read past the head. Content-Length and chunked framing are decoded
/// exactly; unframed bodies are read until EOF. All reads respect
/// `deadline`; exceeding it (or losing the connection early) is a transport
/// failure.
async fn read_response_body(
    stream: &mut TcpStream,
    headers: &[ParsedHeader],
    mut body: Vec<u8>,
    deadline: tokio::time::Instant,
) -> Result<Vec<u8>, ()> {
    let is_chunked = headers.iter().any(|(name, value)| {
        name == "transfer-encoding"
            && String::from_utf8_lossy(value)
                .to_ascii_lowercase()
                .contains("chunked")
    });
    let content_length = headers
        .iter()
        .find(|(name, _)| name == "content-length")
        .and_then(|(_, value)| std::str::from_utf8(value).ok())
        .and_then(|v| v.trim().parse::<usize>().ok());

    if is_chunked {
        let mut out: Vec<u8> = Vec::new();
        let mut rest: Vec<u8> = std::mem::take(&mut body);
        loop {
            let size_line = take_line(&mut rest, stream, 128, deadline).await?;
            let size_hex = std::str::from_utf8(&size_line)
                .ok()
                .and_then(|s| s.split(';').next())
                .map(str::trim)
                .unwrap_or("");
            let size = usize::from_str_radix(size_hex, 16).map_err(|_| ())?;
            // Audit malkuth-R2-3: reject oversized chunks and cap the
            // total accumulated body — the raw-TCP proxy previously
            // buffered without bound (same class as the reqwest side).
            const MAX_PROXY_RESPONSE_BYTES: usize = 32 * 1024 * 1024;
            if size > MAX_PROXY_RESPONSE_BYTES || out.len() + size > MAX_PROXY_RESPONSE_BYTES {
                return Err(());
            }
            if size == 0 {
                // Trailer section, terminated by a blank line.
                loop {
                    if take_line(&mut rest, stream, 4096, deadline)
                        .await?
                        .is_empty()
                    {
                        break;
                    }
                }
                break;
            }
            while rest.len() < size + 2 {
                let n = read_some(stream, &mut rest, deadline).await?;
                if n == 0 {
                    return Err(());
                }
            }
            out.extend_from_slice(&rest[..size]);
            rest.drain(..size + 2); // payload + trailing CRLF
        }
        Ok(out)
    } else if let Some(len) = content_length {
        while body.len() < len {
            let n = read_some(stream, &mut body, deadline).await?;
            if n == 0 {
                return Err(());
            }
        }
        body.truncate(len);
        Ok(body)
    } else {
        loop {
            if read_some(stream, &mut body, deadline).await? == 0 {
                return Ok(body);
            }
        }
    }
}

/// Take one `\r\n`-terminated line, draining `rest` first and topping up
/// from the stream as needed. Line length is capped to protect against
/// hostile framing.
async fn take_line(
    rest: &mut Vec<u8>,
    stream: &mut TcpStream,
    cap: usize,
    deadline: tokio::time::Instant,
) -> Result<Vec<u8>, ()> {
    loop {
        if let Some(pos) = rest.windows(2).position(|w| w == b"\r\n") {
            let line: Vec<u8> = rest.drain(..pos).collect();
            rest.drain(..2);
            if line.len() > cap {
                return Err(());
            }
            return Ok(line);
        }
        if rest.len() > cap {
            return Err(());
        }
        if read_some(stream, rest, deadline).await? == 0 {
            return Err(());
        }
    }
}

/// Read one chunk from the backend into `buf`, respecting `deadline`.
async fn read_some(
    stream: &mut TcpStream,
    buf: &mut Vec<u8>,
    deadline: tokio::time::Instant,
) -> Result<usize, ()> {
    let mut chunk = [0u8; 8192];
    let n = tokio::time::timeout_at(deadline, stream.read(&mut chunk))
        .await
        .map_err(|_| ())?
        .map_err(|_| ())?;
    if n > 0 {
        buf.extend_from_slice(&chunk[..n]);
    }
    Ok(n)
}

async fn proxy_to_backend(req: Request, backend: &str, epoch: &str) -> Result<Response, ()> {
    let (parts, body) = req.into_parts();
    proxy_to_backend_parts(parts, body, backend, epoch).await
}

/// Plain-HTTP forward of a request to the `--serve` backend (reqwest; no
/// upgrade semantics). Consumed by both [`proxy_to_backend`] and the
/// no-upgrade fallback inside [`proxy_upgrade`].
async fn proxy_to_backend_parts(
    parts: axum::http::request::Parts,
    body: axum::body::Body,
    backend: &str,
    epoch: &str,
) -> Result<Response, ()> {
    let path = parts.uri.path_and_query().map_or("/", |pq| pq.as_str());
    let url = format!("{}{}", backend.trim_end_matches('/'), path);

    let client = crate::http_client_builder()
        .no_proxy()
        // Never follow redirects server-side: a 3xx must reach the browser
        // untouched. Following an absolute redirect could loop back through
        // this proxy and surface the landing page as a bogus 200.
        .redirect(reqwest::redirect::Policy::none())
        .connect_timeout(std::time::Duration::from_millis(500))
        .timeout(std::time::Duration::from_secs(30))
        .build()
        .map_err(|_| ())?;

    let method = parts.method;
    let headers = parts.headers;

    let body_bytes = read_proxied_body(body).await?;

    let mut backend_req = match method.as_str() {
        "GET" => client.get(&url),
        "POST" => client.post(&url),
        "PUT" => client.put(&url),
        "DELETE" => client.delete(&url),
        "PATCH" => client.patch(&url),
        "HEAD" => client.head(&url),
        _ => client.get(&url),
    };

    for (name, value) in headers.iter() {
        let lower = name.as_str().to_lowercase();
        if lower == "cookie" {
            if let Ok(cookies) = value.to_str() {
                let cleaned: Vec<&str> = cookies
                    .split(';')
                    .filter(|c| !c.trim().starts_with("__malkuth_nonce="))
                    .collect();
                if !cleaned.is_empty() {
                    backend_req = backend_req.header(name.as_str(), cleaned.join(";").as_bytes());
                }
            }
        } else if lower != "host" && lower != "connection" && lower != "transfer-encoding" {
            backend_req = backend_req.header(name.as_str(), value.as_bytes());
        }
    }

    if !body_bytes.is_empty() {
        backend_req = backend_req.body(body_bytes.to_vec());
    }

    let resp = backend_req.send().await.map_err(|_| ())?;
    let status_code = resp.status().as_u16();

    // Fall back to the landing page only on transport failure (above) or
    // gateway-style unavailability (502/503/504 — see `should_fall_back`,
    // aligned with the probe's 503 → NotReady verdict). Every other status
    // — 404, 3xx, even 500 — is a live backend answer and is passed through
    // to the browser untouched; masking it as "offline" is what trapped
    // users on the landing page when a backend 404'd on `/`.
    if should_fall_back(status_code) {
        return Err(());
    }

    let status = StatusCode::from_u16(status_code).unwrap_or(StatusCode::OK);
    // Keep backend cookies separate so the malkuth nonce cookie never
    // clobbers them (HeaderMap::insert would replace all Set-Cookie values).
    let mut backend_cookies: Vec<Vec<u8>> = Vec::new();
    let resp_headers: Vec<(String, Vec<u8>)> = resp
        .headers()
        .iter()
        .filter(|(n, v)| {
            let lower = n.as_str().to_lowercase();
            if lower == "set-cookie" {
                backend_cookies.push(v.as_bytes().to_vec());
                return false;
            }
            lower != "transfer-encoding" && lower != "connection"
        })
        .map(|(n, v)| (n.as_str().to_string(), v.as_bytes().to_vec()))
        .collect();
    // Audit malkuth-R2-3: cap the proxied response body — a
    // compromised pod can stream an unbounded body and OOM the
    // supervisor (the request side was capped in #153; this closes
    // the response side).
    const MAX_PROXY_RESPONSE_BYTES: u64 = 32 * 1024 * 1024;
    if let Some(len) = resp.content_length() {
        if len > MAX_PROXY_RESPONSE_BYTES {
            return Err(());
        }
    }
    let resp_body = resp.bytes().await.map_err(|_| ())?;
    if resp_body.len() as u64 > MAX_PROXY_RESPONSE_BYTES {
        return Err(());
    }

    let mut response = Response::new(axum::body::Body::from(resp_body));
    *response.status_mut() = status;

    response
        .headers_mut()
        .append(header::SET_COOKIE, nonce_cookie(epoch));
    for value in backend_cookies {
        if let Ok(v) = header::HeaderValue::from_bytes(&value) {
            response.headers_mut().append(header::SET_COOKIE, v);
        }
    }

    for (name, value) in resp_headers {
        if let (Ok(n), Ok(v)) = (
            header::HeaderName::from_bytes(name.as_bytes()),
            header::HeaderValue::from_bytes(&value),
        ) {
            response.headers_mut().insert(n, v);
        }
    }

    Ok(response)
}

/// Read the raw `__malkuth_nonce` cookie value ("" = absent). The value is
/// compared against the backend's current build token: matching means the
/// client has already seen (and been redirected past the landing page for)
/// the build currently served.
fn read_nonce(req: &Request) -> String {
    let cookie_header = req.headers().get(header::COOKIE);
    let Some(cookies) = cookie_header.and_then(|v| v.to_str().ok()) else {
        return String::new();
    };
    for part in cookies.split(';') {
        let kv = part.trim();
        if let Some(val) = kv.strip_prefix("__malkuth_nonce=") {
            return val.trim().to_string();
        }
    }
    String::new()
}

/// Whether a request that lacks a token for the current build should be
/// answered with the landing interstitial instead of being forwarded.
/// Only human-facing document loads qualify — they are the requests whose
/// response renders as a page, so serving the landing there steers the
/// client into the fresh build (countdown → cookie → reload). Everything
/// else (API/XHR calls, POSTs, probes, asset fetches with `Accept: */*`)
/// is forwarded so a previous-build session and machine clients keep
/// working.
fn request_interceptable(req: &Request) -> bool {
    if !matches!(
        *req.method(),
        axum::http::Method::GET | axum::http::Method::HEAD
    ) {
        return false;
    }
    if let Some(dest) = req
        .headers()
        .get("sec-fetch-dest")
        .and_then(|v| v.to_str().ok())
    {
        return matches!(dest, "document" | "iframe");
    }
    req.headers()
        .get(header::ACCEPT)
        .and_then(|v| v.to_str().ok())
        .is_some_and(|a| a.to_ascii_lowercase().contains("text/html"))
}

/// JSON probe endpoint for polling landing page.
async fn serve_probe(lang: &str, state: &InfoState, allowed: bool) -> Response {
    let backend_state = *state.backend_state.read().await;
    let backend_up = state.serve_backend.is_some() && backend_state == BackendState::Up;

    // Serve doors keep the probe's redirect-oriented shape (`ready` when the
    // backend answers: the SPA turns that into a stamp+reload). Supervisor
    // doors take the same state the document renders, so a rebuild reports
    // "building" here instead of flipping the page to a green "ready" on the
    // next poll.
    let (probe_state, msg, probe_key) = if state.serve_backend.is_some() {
        if backend_up {
            ("ready", "", "status_ready")
        } else if backend_state == BackendState::Unknown {
            ("landing", "", "status_landing")
        } else if backend_state == BackendState::NotReady {
            ("starting", "", "status_starting")
        } else {
            ("offline", "", "status_offline")
        }
    } else {
        let (state_name, key) = page_state(state, backend_state);
        (state_name, "", key)
    };

    let message = if msg.is_empty() {
        let i18n = get_i18n(lang);
        i18n.get(probe_key)
            .map(|v| v.to_string())
            .unwrap_or_else(|| msg.to_string())
    } else {
        msg.to_string()
    };

    let progress = state.build_progress.lock().ok().and_then(|g| g.clone());
    let runtime_log: Vec<String> = state
        .runtime_log
        .lock()
        .ok()
        .map(|g| g.clone())
        .unwrap_or_default();
    let build_log: Vec<String> = state
        .build_log
        .lock()
        .ok()
        .map(|g| g.clone())
        .unwrap_or_default();
    let log: Vec<String> = if !runtime_log.is_empty() {
        runtime_log
    } else {
        build_log
    };
    // The VTty badge name derives from the program PATH (the collected
    // BinaryInfo took its name from the same path anyway) — the probe runs
    // every few seconds while clients poll, so it must not touch the disk.
    let vtty_name = state
        .binary_programs
        .first()
        .and_then(|p| {
            std::path::Path::new(p)
                .file_name()
                .map(|n| n.to_string_lossy().into_owned())
        })
        .unwrap_or_default();

    let json = serde_json::json!({
        "state": probe_state,
        "message": message,
        "progress": progress,
        "epoch": state.epoch.read().await.clone(),
        // Whether the polling client may act on "ready" with a stamp+reload:
        // only doors that will actually proxy this origin qualify (see
        // `info_page`'s `serve_allowed`). Without this a landing page served
        // by a non-proxy door (no --serve, or a --serve-host mismatch) would
        // reload itself in a loop, since its reload can never reach a backend.
        "serve": state.serve_backend.is_some() && allowed,
        "vttys": [{
            "name": vtty_name,
            "log": log,
        }],
    })
    .to_string();

    let mut resp = Response::new(axum::body::Body::from(json));
    resp.headers_mut().insert(
        header::CONTENT_TYPE,
        header::HeaderValue::from_static("application/json"),
    );
    resp
}

/// One page state, shared by the SPA init payload and the fallback renderer.
///
/// `serve_backend` doors report the BACKEND's health (the supervised service is
/// what the visitor came for); supervisor doors report their own task status.
fn page_state(state: &InfoState, backend: BackendState) -> (&'static str, &'static str) {
    if state.serve_backend.is_some() {
        return match backend {
            BackendState::Up | BackendState::Unknown => ("landing", "status_landing"),
            BackendState::NotReady => ("starting", "status_starting"),
            BackendState::Down => ("offline", "status_offline"),
        };
    }
    match state.status {
        InfoStatus::Ready => ("ready", "status_ready"),
        InfoStatus::Working => ("building", "status_building"),
        InfoStatus::Landing => ("landing", "status_landing"),
    }
}

/// hikari badge variant for a page state (success / warning / error).
fn badge_variant(page_state: &str) -> &'static str {
    match page_state {
        "ready" => "success",
        "offline" => "error",
        _ => "warning",
    }
}

/// Auto-refresh delay for the no-JS fallback page, and whether it applies:
/// an offline/starting backend gets the manual action only (the page would
/// otherwise reload itself forever while the service is down).
const FALLBACK_RETRY_SECS: u32 = 10;

/// Redirecting a visitor into a reachable backend is a 3s beat (master's
/// landing countdown); waiting out a rebuild is the slower retry cadence.
const FALLBACK_REDIRECT_SECS: u32 = 3;

/// Does this state self-refresh at all? This is the STATE axis only — the
/// capability axis (may a reload actually reach a backend?) is ANDed in by
/// [`serve_fallback`], because a non-proxy door that reloads would loop.
fn state_wants_refresh(page_state: &str) -> bool {
    page_state != "offline" && page_state != "starting"
}

/// Render the prerendered, no-JS fallback page.
///
/// The markup is the build-time SSR of the SAME hikari component tree the SPA
/// renders (see packages/landing_page/src/ssr/prerender.ts), so the two
/// surfaces cannot drift apart. This function only fills the per-request
/// values — every substitution is escaped for the context it lands in — and
/// drops the optional blocks.
async fn serve_fallback(lang: &str, state: &InfoState, serve_allowed: bool) -> Response {
    Html(render_fallback_page(get_i18n(lang), lang, state, serve_allowed).await).into_response()
}

/// Core of the fallback renderer, with the catalogue injected so tests can
/// feed hostile translations.
///
/// Escaping invariant: every catalogue/state string goes through `text()` or
/// `html_escape()` exactly once; the only raw slots are the structural blocks
/// the renderer builds itself (badge/rows/retry/action/meta) and the
/// whitelisted `lang`/`dir` values.
async fn render_fallback_page(
    i18n: &HashMap<String, String>,
    lang: &str,
    state: &InfoState,
    serve_allowed: bool,
) -> String {
    let binaries = collect_binaries(&state.binary_programs).await;
    let backend = *state.backend_state.read().await;
    let (page, status_key) = page_state(state, backend);
    // ONE escaping point for every catalogue string: `raw` is the catalogue
    // value, `text` is its escaped HTML form. Helpers that escape their own
    // argument (kv_row, the row builders) take `raw`; everything interpolated
    // straight into the page takes `text`.
    let raw = |key: &str| -> String { i18n.get(key).cloned().unwrap_or_default() };
    let text = |key: &str| -> String { html_escape(&raw(key)) };
    let dir = if lang == "ar" { "rtl" } else { "ltr" };
    // Reloading can only carry the visitor into the backend when this origin
    // may be proxied at all; the SPA applies the same guard through
    // `init.serve`. Without it a --serve-host mismatch door would reload
    // itself forever (the exact loop `build_spa_init` warns about).
    let auto_refresh = serve_allowed && state_wants_refresh(page);
    let redirecting = page == "landing";
    let refresh_secs = if redirecting {
        FALLBACK_REDIRECT_SECS
    } else {
        FALLBACK_RETRY_SECS
    };

    let status_badge = format!(
        r#"<span class="hk-badge hk-badge-{}" data-size="md" data-pill><span data-el="dot"></span>{}</span>"#,
        badge_variant(page),
        text(status_key)
    );

    // Supervisor doors also list their current task; service doors do not
    // have one (their "task" is the backend's own health).
    let task_row = if state.serve_backend.is_none() {
        let task = match state.status {
            InfoStatus::Ready => text("task_idle"),
            InfoStatus::Working => text("task_working"),
            InfoStatus::Landing => text("task_landing"),
        };
        kv_row(&raw("task"), &task)
    } else {
        String::new()
    };

    let proxy_row = match state.proxy_endpoint.as_deref() {
        Some(endpoint) => kv_row(&raw("proxy_label"), &html_escape(endpoint)),
        None => String::new(),
    };

    let watch_row = if state.watch_paths.is_empty() {
        String::new()
    } else {
        let mut chips = String::new();
        for path in &state.watch_paths {
            chips.push_str(&format!(
                r#"<span class="hk-badge hk-badge-default mk-chip" data-size="sm" data-mono>{}</span>"#,
                html_escape(path)
            ));
        }
        format!(
            r#"<div class="mk-kv-row"><dt>{}</dt><dd class="mk-chips">{}</dd></div>"#,
            text("watch_label"),
            chips
        )
    };

    let binaries_block = if binaries.is_empty() {
        String::new()
    } else {
        let mut rows = String::new();
        for binary in &binaries {
            rows.push_str(&format!(
                r#"<div class="hk-status-row hk-status-row--info"><span class="hk-status-row__chip"></span><div class="hk-status-row__body"><span class="hk-status-row__title">{name}</span><span class="hk-status-row__status">{time}</span></div><div class="hk-status-row__action"><span class="hk-badge hk-badge-default" data-size="sm" data-mono>{short}</span></div></div>"#,
                name = html_escape(&binary.name),
                time = html_escape(&binary.compile_time),
                short = html_escape(&binary.hash_short),
            ));
        }
        format!(
            r#"<section class="mk-binaries"><h2 class="mk-section-title">{title}</h2><div class="mk-binary-list">{rows}</div></section>"#,
            title = text("binaries_title"),
        )
    };

    let retry_block = if auto_refresh {
        // Redirecting states keep the landing copy ("Redirecting in 3
        // seconds"); waiting out a rebuild uses the retry copy ("Page will
        // refresh automatically in 10 seconds"). `{n}` is the number slot.
        let (before, after) = if redirecting {
            (text("redirect_before"), text("redirect_after"))
        } else {
            // `retry` ends at `{n}` in several locales; the unit lives in its
            // own key, so splice it when the sentence has no tail of its own.
            let retry = text("retry");
            match retry.split_once("{n}") {
                Some((before, after)) if after.trim().is_empty() => {
                    (before.to_string(), format!(" {}", text("retry_unit")))
                }
                Some((before, after)) => (before.to_string(), after.to_string()),
                None => (retry.clone(), String::new()),
            }
        };
        format!(
            r#"<p class="mk-retry"><span>{}</span><strong class="mk-countdown">{refresh_secs}</strong><span>{}</span></p>"#,
            before, after
        )
    } else {
        String::new()
    };

    let auto_refresh_meta = if auto_refresh {
        format!(r#"<meta http-equiv="refresh" content="{refresh_secs}">"#)
    } else {
        String::new()
    };

    let action_block = format!(
        r#"<div class="mk-actions"><a class="hk-btn hk-btn-primary hk-btn-sm" href=""><span class="hk-btn-label">{}</span></a></div>"#,
        text("refresh_label")
    );

    // Keep the install-provenance line the hand-written page used to show
    // (`v0.6.0 (Installed via cargo)`); the SPA shows the bare version.
    let install_method = detect_install_method();
    let version_line = if install_method.is_empty() {
        format!("v{}", html_escape(&state.version))
    } else {
        format!(
            "v{} ({} {})",
            html_escape(&state.version),
            text("install_label"),
            html_escape(install_method)
        )
    };

    let slots: [(&str, String); 16] = [
        ("{{MALKUTH_LANG}}", lang.to_string()),
        ("{{MALKUTH_DIR}}", dir.to_string()),
        ("{{MALKUTH_TITLE}}", text("title")),
        ("{{MALKUTH_AUTO_REFRESH}}", auto_refresh_meta),
        (
            "{{MALKUTH_LOGO_SRC}}",
            format!("data:image/webp;base64,{}", base64_encode(LOGO_BYTES)),
        ),
        ("{{MALKUTH_HEADING}}", text("heading")),
        ("{{MALKUTH_TAGLINE}}", text("tagline")),
        ("{{MALKUTH_STATUS_BADGE}}", status_badge),
        ("{{MALKUTH_TASK_ROW}}", task_row),
        ("{{MALKUTH_PROXY_ROW}}", proxy_row),
        ("{{MALKUTH_WATCH_ROW}}", watch_row),
        ("{{MALKUTH_BINARIES_BLOCK}}", binaries_block),
        ("{{MALKUTH_RETRY_BLOCK}}", retry_block),
        ("{{MALKUTH_ACTION_BLOCK}}", action_block),
        ("{{MALKUTH_FOOTER}}", text("footer")),
        ("{{MALKUTH_VERSION_LINE}}", version_line),
    ];
    let refs: Vec<(&str, &str)> = slots
        .iter()
        .map(|(name, value)| (*name, value.as_str()))
        .collect();
    fill(FALLBACK_PAGE_HTML, &refs)
}

/// One label/value row in the fallback page (`value` is already escaped HTML).
fn kv_row(label: &str, value: &str) -> String {
    format!(
        r#"<div class="mk-kv-row"><dt>{}</dt><dd class="mk-kv-value">{}</dd></div>"#,
        html_escape(label),
        value
    )
}

/// Single-pass `{{MALKUTH_*}}` substitution.
///
/// One pass only: substituted values are never re-scanned, so a value that
/// happens to contain a marker cannot rewrite it. Unknown markers are left
/// verbatim (build.rs already pins the marker set; the test below asserts the
/// rendered page contains none).
fn fill(template: &str, slots: &[(&str, &str)]) -> String {
    let mut out = String::with_capacity(template.len() + 512);
    let mut rest = template;
    while let Some(idx) = rest.find("{{MALKUTH_") {
        out.push_str(&rest[..idx]);
        let after = &rest[idx..];
        match after.find("}}") {
            Some(end) => {
                let marker = &after[..end + 2];
                match slots.iter().find(|(name, _)| *name == marker) {
                    Some((_, value)) => out.push_str(value),
                    None => out.push_str(marker),
                }
                rest = &after[end + 2..];
            }
            None => {
                out.push_str(after);
                rest = "";
            }
        }
    }
    out.push_str(rest);
    out
}

/// HTML text/attribute escaping (`& < > " '`).
fn html_escape(value: &str) -> String {
    let mut out = String::with_capacity(value.len());
    for ch in value.chars() {
        match ch {
            '&' => out.push_str("&amp;"),
            '<' => out.push_str("&lt;"),
            '>' => out.push_str("&gt;"),
            '"' => out.push_str("&quot;"),
            '\'' => out.push_str("&#39;"),
            c => out.push(c),
        }
    }
    out
}

/// JSON safe to embed inside a `<script>` element: `<`, `>`, `&` are escaped
/// to their `\uXXXX` forms, so no payload value can close the element early.
fn init_json_script(value: &serde_json::Value) -> String {
    let raw = serde_json::to_string(value).unwrap_or_else(|_| "null".to_string());
    raw.replace('<', "\\u003c")
        .replace('>', "\\u003e")
        .replace('&', "\\u0026")
}

async fn info_page(state: axum::extract::State<InfoState>, req: Request) -> Response {
    let lang = detect_language(req.headers());

    // Whether this request's origin (Host header) may be proxied to the
    // --serve backend. Computed once, up front, so the JSON probe and the
    // SPA init report the same capability the document path enforces —
    // a landing page told "serve: false" must never assume a reload can
    // carry it into the backend (no --serve backend configured, or a
    // --serve-host restriction excluding this origin), or its poll-driven
    // ready-reload would loop forever on a page that can only re-render
    // itself.
    let serve_allowed = state.serve_hosts.is_empty()
        || state.serve_hosts.iter().any(|h| {
            req.headers()
                .get(header::HOST)
                .is_some_and(|v| v.as_bytes() == h.as_bytes())
        });

    if req.headers().get("x-malkuth-probe").is_some() {
        return serve_probe(&lang, &state, serve_allowed).await;
    }

    if let Some(ref backend) = state.serve_backend {
        let allowed = serve_allowed;
        if allowed {
            let epoch = state.epoch.read().await.clone();
            let nonce = read_nonce(&req);
            let seen_current_build = !nonce.is_empty() && nonce == epoch;
            // Requests without a token for the current build still pass
            // through when they cannot render the interstitial: WebSocket
            // upgrades, and non-document API/XHR traffic — a still-running
            // SPA from the previous build must keep functioning until its
            // next document navigation, which IS intercepted and funnels
            // the client through the landing page into the new build.
            let intercept = !seen_current_build
                && !is_upgrade_request(req.headers())
                && request_interceptable(&req);
            if !intercept {
                // Upgrade handshakes (WebSocket etc.) cannot go through the
                // reqwest forwarder — detect them from the headers before
                // consuming the request and tunnel them instead.
                let proxy_result = if is_upgrade_request(req.headers()) {
                    proxy_upgrade(req, backend, &epoch).await
                } else {
                    proxy_to_backend(req, backend, &epoch).await
                };
                if let Ok(resp) = proxy_result {
                    return resp;
                }
            }
            return serve_document(&lang, &state, allowed).await;
        }
    }

    serve_document(&lang, &state, serve_allowed).await
}

/// The document a browser gets: the embedded SPA when one was built, the
/// prerendered fallback page otherwise. `allowed` says whether this origin may
/// be proxied into a --serve backend (the SPA's poll-driven reload honours it).
async fn serve_document(lang: &str, state: &InfoState, allowed: bool) -> Response {
    if spa_available() {
        serve_spa(state, lang, allowed).await
    } else {
        serve_fallback(lang, state, allowed).await
    }
}

const SUPPORTED: &[&str] = &[
    "en", "zh-Hans", "zh-Hant", "ja", "ko", "fr", "de", "es", "pt", "ru", "ar",
];

fn detect_language(headers: &header::HeaderMap) -> String {
    let raw = headers
        .get(header::ACCEPT_LANGUAGE)
        .and_then(|v| v.to_str().ok())
        .unwrap_or("en");

    let mut best_quality = 0.0f32;
    let mut best_lang = "en".to_string();

    for part in raw.split(',') {
        let part = part.trim();
        if part.is_empty() {
            continue;
        }
        let (tag, q) = part.split_once(';').unwrap_or((part, "q=1.0"));
        let quality: f32 = q
            .trim()
            .strip_prefix("q=")
            .and_then(|s| s.parse().ok())
            .unwrap_or(1.0);

        if quality <= best_quality {
            continue;
        }

        let lang = tag.trim();
        let matched = match_language(lang);
        best_quality = quality;
        best_lang = matched;
    }

    best_lang
}

fn match_language(tag: &str) -> String {
    let tag_lower = tag.to_lowercase();
    if SUPPORTED.contains(&tag_lower.as_str()) {
        return tag_lower;
    }
    match tag_lower.as_str() {
        "zh" | "zh-cn" | "zh-hans" | "zh-sg" => "zh-Hans".into(),
        "zh-tw" | "zh-hk" | "zh-mo" | "zh-hant" => "zh-Hant".into(),
        other => {
            let base = other.split_once('-').map_or(other, |(b, _)| b);
            if SUPPORTED.contains(&base) {
                base.into()
            } else {
                "en".into()
            }
        }
    }
}

fn get_i18n(lang: &str) -> &HashMap<String, String> {
    static EMPTY: LazyLock<HashMap<String, String>> = LazyLock::new(HashMap::new);
    I18N_DATA
        .get(lang)
        .unwrap_or_else(|| I18N_DATA.get("en").unwrap_or(&EMPTY))
}

#[cfg(test)]
mod tests {
    use super::*;

    // ── Family build identity (`<branch>:<hash7>`) on the landing page ──

    /// Unique temp path for a fake supervised binary.
    fn temp_binary(tag: &str) -> std::path::PathBuf {
        std::env::temp_dir().join(format!(
            "malkuth-identity-{tag}-{}-{}",
            std::process::id(),
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .unwrap()
                .as_nanos()
        ))
    }

    // ── Proxy body cap (`MALKUTH_PROXY_MAX_BODY_BYTES`) ──────────────────

    /// The R1-6 posture is the default and stays byte-for-byte what it was: an
    /// instance that configures nothing must behave exactly as before the knob
    /// existed.
    #[test]
    fn test_proxy_body_cap_default_is_unchanged() {
        assert_eq!(DEFAULT_MAX_PROXY_BODY_BYTES, 1024 * 1024);
        // Unset, empty and whitespace-only all mean "not configured".
        for raw in [None, Some(""), Some("   ")] {
            assert_eq!(
                resolve_max_proxy_body_bytes(raw),
                DEFAULT_MAX_PROXY_BODY_BYTES,
                "raw={raw:?} must fall back to the R1-6 default"
            );
        }
    }

    /// The override is a plain byte count; surroundings are tolerated because
    /// they arrive from systemd/env files, where a stray newline is normal.
    #[test]
    fn test_proxy_body_cap_override_parses_bytes() {
        assert_eq!(
            resolve_max_proxy_body_bytes(Some("12582912")),
            12 * 1024 * 1024,
            "the documented ≈12 MiB figure for chest's 10 MiB /api face"
        );
        assert_eq!(
            resolve_max_proxy_body_bytes(Some(" 2097152 ")),
            2 * 1024 * 1024
        );
        assert_eq!(resolve_max_proxy_body_bytes(Some("1")), 1);
    }

    /// Negative controls: a 0 cap would reject every request that carries a
    /// body (a typo that takes the face down), and junk is not a byte count.
    /// Both are refused — and refused by keeping the default, which is why the
    /// resolution is loud in the log.
    #[test]
    fn test_proxy_body_cap_refuses_zero_and_junk() {
        for bad in ["0", "00", "-1", "12m", "12MB", "1.5", "abc", "0x10"] {
            assert_eq!(
                resolve_max_proxy_body_bytes(Some(bad)),
                DEFAULT_MAX_PROXY_BODY_BYTES,
                "{bad:?} must be refused, not honoured"
            );
        }
    }

    /// The cap is enforced where it is APPLIED, tested through the seam with
    /// small caps: exactly-at passes, one byte over is refused.
    ///
    /// Small caps on purpose — a test that used the resolved process cap would
    /// have to allocate it (the doc recommends up to ~514 MiB for model
    /// uploads), which is both slow and a denial-of-service on the test runner
    /// when the suite happens to run with a large override.
    #[tokio::test]
    async fn test_proxied_body_over_the_cap_is_rejected() {
        for cap in [8usize, 64 * 1024, DEFAULT_MAX_PROXY_BODY_BYTES] {
            assert!(
                read_proxied_body_with(axum::body::Body::from(vec![0u8; cap]), cap)
                    .await
                    .is_ok(),
                "a body of exactly {cap} bytes is within the cap"
            );
            assert!(
                read_proxied_body_with(axum::body::Body::from(vec![0u8; cap + 1]), cap)
                    .await
                    .is_err(),
                "a body of {} bytes must be refused at cap {cap}",
                cap + 1
            );
        }
    }

    /// …and the production reader is WIRED to the knob, not to the default
    /// constant. Without this pin the knob can go dead in a refactor while every
    /// other test stays green: the seam above would keep passing against a
    /// literal, and the parse tests never touch the reader.
    ///
    /// Structural, because the alternative — setting the process environment
    /// before the first resolve — is global and racy under a threaded runner.
    #[test]
    fn test_the_proxy_reader_uses_the_configured_cap() {
        let source = include_str!("info_page.rs");
        // Read only the production half: a pin that also scanned the tests
        // would match its own literals and pass vacuously.
        let production = &source[..source
            .find("\n#[cfg(test)]\nmod tests")
            .expect("the test module marker moved")];
        let start = production
            .find("async fn read_proxied_body(")
            .expect("the reader must exist");
        let body = &production[start..start + production[start..].find("\n}\n").expect("body")];
        assert!(
            body.contains("max_proxy_body_bytes()"),
            "the production reader must resolve the cap at request time: {body}"
        );
    }

    /// No upper bound is enforced (an explicit operator number is honoured),
    /// but the documented class is a known ceiling rather than a silent one.
    #[test]
    fn test_proxy_body_cap_honours_large_values_and_knows_the_documented_class() {
        // The documented ceiling is chest's 512 MiB model-upload class plus
        // slack; a cap AT it is ordinary configuration and is honoured as-is.
        assert_eq!(
            resolve_max_proxy_body_bytes(Some(&LARGEST_DOCUMENTED_CLASS_BYTES.to_string())),
            LARGEST_DOCUMENTED_CLASS_BYTES,
            "the documented ceiling must be honoured exactly"
        );
        assert_eq!(
            LARGEST_DOCUMENTED_CLASS_BYTES,
            514 * 1024 * 1024,
            "512 MiB model uploads plus slack"
        );
        // Above it: honoured (the warn is the guard rail, not a refusal), so an
        // operator's explicit number is never silently replaced.
        let huge = 4 * 1024 * 1024 * 1024usize;
        assert_eq!(resolve_max_proxy_body_bytes(Some(&huge.to_string())), huge);
        // And the pathological value `usize` accepts is still honoured — that
        // is precisely why the warning exists.
        assert_eq!(
            resolve_max_proxy_body_bytes(Some("18446744073709551615")),
            usize::MAX
        );
    }

    #[test]
    fn test_scan_extracts_single_family_identity() {
        // A family binary states its plana_build_info version line among
        // the surrounding binary junk.
        let bytes = b"\x7fELF\x00\x02\x00 0.1.0 master:c350cb4 \x00\x01 trailing".to_vec();
        assert_eq!(
            scan_family_identity(&bytes).as_deref(),
            Some("master:c350cb4")
        );
    }

    #[test]
    fn test_scan_normalizes_full_hash_to_seven() {
        // The full-hash print variant (`0.1.0 master:<40 hex>`) reports the
        // same 7-char identity as the short variant.
        let bytes = b"x 0.1.0 master:c350cb4fa9b2d3e4f5a6b7c8d9e0f1a2b3c4d5e y".to_vec();
        assert_eq!(
            scan_family_identity(&bytes).as_deref(),
            Some("master:c350cb4")
        );
    }

    #[test]
    fn test_scan_accepts_both_separator_generations() {
        // The family narrowed `::` → `:` on 2026-10-11; transition-window
        // binaries still speak either, and branch names may carry `/`.
        assert_eq!(
            scan_family_identity(b"0.1.0 feat/landing::770f625 end").as_deref(),
            Some("feat/landing:770f625")
        );
        assert_eq!(
            scan_family_identity(b"0.1.0 release:1a2b3c4").as_deref(),
            Some("release:1a2b3c4")
        );
    }

    #[test]
    fn test_scan_rejects_absent_ambiguous_and_marker_identities() {
        // No stamp at all.
        assert_eq!(scan_family_identity(b"1.2.3 no identity here"), None);
        // Registry-built dependency markers resolve to non-hex and must
        // never pose as an identity.
        assert_eq!(scan_family_identity(b"0.1.26 detached:unknown"), None);
        // Hash shorter than 7 hex chars does not match.
        assert_eq!(scan_family_identity(b"0.1.0 x:abcdef"), None);
        // Two DIFFERENT identities is ambiguous — never guess.
        assert_eq!(
            scan_family_identity(b"0.1.0 master:aaaaaaa and 0.2.0 release:bbbbbbb"),
            None
        );
        // Repeats of the SAME identity (env! used in several places) stay
        // unambiguous.
        assert_eq!(
            scan_family_identity(b"0.1.0 master:c350cb4 0.1.0 master:c350cb4fa99"),
            Some("master:c350cb4".to_string())
        );
    }

    #[test]
    fn test_collect_prefers_stated_identity_over_content_hash() {
        let path = temp_binary("stamped");
        std::fs::write(&path, b"\x00\x00 0.1.0 master:c350cb4 \x00").unwrap();
        let info = collect_binary_info(path.to_str().unwrap()).unwrap();
        assert_eq!(info.hash, "master:c350cb4");
        assert_eq!(info.hash_short, "master:c350cb4");
        assert_eq!(info.name, path.file_name().unwrap().to_str().unwrap());
        assert!(!info.compile_time.is_empty());
        std::fs::remove_file(&path).ok();
    }

    #[test]
    fn test_collect_falls_back_to_content_fingerprint() {
        let path = temp_binary("plain");
        std::fs::write(&path, b"an ordinary binary with no family line").unwrap();
        let info = collect_binary_info(path.to_str().unwrap()).unwrap();
        // Legacy shape: base32 (RFC 4648 padded to a multiple of 8), the
        // short form = last 6 chars of the unpadded tail.
        assert!(
            info.hash
                .chars()
                .all(|c| { c.is_ascii_uppercase() || c.is_ascii_digit() || c == '=' })
        );
        assert_eq!(info.hash.len() % 8, 0);
        let trimmed = info.hash.trim_end_matches('=');
        assert_eq!(info.hash_short, &trimmed[trimmed.len() - 6..]);
        assert_ne!(info.hash, info.hash_short);
        std::fs::remove_file(&path).ok();
    }

    #[test]
    fn test_collect_refreshes_when_the_binary_changes() {
        // Deploys swap the watched binary under a running malkuth — the
        // mtime+size memo must invalidate and the next collect must state
        // the identity of the file that is there NOW.
        let path = temp_binary("refresh");
        std::fs::write(&path, format!("{:<64}", "0.1.0 master:aaaaaaa")).unwrap();
        let first = collect_binary_info(path.to_str().unwrap()).unwrap();
        assert_eq!(first.hash, "master:aaaaaaa");

        // Same length, later mtime: only the mtime key can catch it.
        let later = std::time::SystemTime::now() + std::time::Duration::from_secs(1);
        std::fs::write(&path, format!("{:<64}", "0.1.0 master:bbbbbbb")).unwrap();
        std::fs::File::options()
            .write(true)
            .open(&path)
            .unwrap()
            .set_modified(later)
            .unwrap();
        let second = collect_binary_info(path.to_str().unwrap()).unwrap();
        assert_eq!(second.hash, "master:bbbbbbb");
        std::fs::remove_file(&path).ok();
    }

    /// Serve a landing router over a real listener; returns the door URL.
    async fn spawn_landing_door(program: String) -> String {
        let router = info_router(
            "test".to_string(),
            InfoStatus::Landing,
            vec![],
            None,
            vec![program],
            None,
            vec![],
            std::sync::Arc::new(std::sync::Mutex::new(None)),
            std::sync::Arc::new(std::sync::Mutex::new(vec![])),
            std::sync::Arc::new(std::sync::Mutex::new(vec![])),
        );
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let door = listener.local_addr().unwrap();
        tokio::spawn(async move {
            let _ = axum::serve(listener, router).await;
        });
        format!("http://{door}")
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn test_landing_page_states_the_binary_identity_per_render() {
        let path = temp_binary("e2e");
        std::fs::write(&path, format!("{:<96}", "0.1.0 master:c350cb4")).unwrap();
        let door = spawn_landing_door(path.to_string_lossy().into_owned()).await;
        let client = crate::http_client_builder().no_proxy().build().unwrap();

        let first = client
            .get(&door)
            .send()
            .await
            .unwrap()
            .text()
            .await
            .unwrap();
        assert!(
            first.contains("master:c350cb4"),
            "landing page must state the binary's family identity"
        );

        // A deploy swaps the binary (same length, later mtime): the NEXT
        // render must state the new identity — no malkuth restart.
        let later = std::time::SystemTime::now() + std::time::Duration::from_secs(1);
        std::fs::write(&path, format!("{:<96}", "0.1.0 master:ddddddd")).unwrap();
        std::fs::File::options()
            .write(true)
            .open(&path)
            .unwrap()
            .set_modified(later)
            .unwrap();
        let second = client
            .get(&door)
            .send()
            .await
            .unwrap()
            .text()
            .await
            .unwrap();
        assert!(second.contains("master:ddddddd"));
        assert!(!second.contains("master:c350cb4"));
        std::fs::remove_file(&path).ok();
    }

    #[test]
    fn test_detect_en() {
        let mut headers = header::HeaderMap::new();
        headers.insert("accept-language", "en-US,en;q=0.9".parse().unwrap());
        assert_eq!(detect_language(&headers), "en");
    }

    /// Spawn a one-shot TCP responder on 127.0.0.1 and return its host:port.
    fn spawn_stub_backend(response: &'static str) -> String {
        spawn_custom_backend(move |mut stream| {
            use std::io::Write;
            let _ = stream.write_all(response.as_bytes());
        })
    }

    /// Spawn a one-shot TCP backend whose behaviour is fully customised.
    /// The closure receives the accepted stream after the request was read.
    fn spawn_custom_backend<F>(f: F) -> String
    where
        F: FnOnce(std::net::TcpStream) + Send + 'static,
    {
        let listener = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
        let port = listener.local_addr().unwrap().port();
        std::thread::spawn(move || {
            if let Ok((mut stream, _)) = listener.accept() {
                use std::io::Read;
                let mut buf = [0u8; 256];
                let _ = stream.read(&mut buf);
                f(stream);
            }
        });
        format!("127.0.0.1:{port}")
    }

    #[test]
    fn test_backend_up_on_any_non503_status() {
        for status in [
            "200 OK",
            "301 Moved Permanently",
            "404 Not Found",
            "500 Boom",
        ] {
            let hp = spawn_stub_backend(Box::leak(
                format!("HTTP/1.1 {status}\r\nContent-Length: 0\r\n\r\n").into_boxed_str(),
            ));
            assert_eq!(
                probe_backend(&hp),
                BackendState::Up,
                "HTTP status `{status}` must count as reachable"
            );
        }
    }

    #[test]
    fn test_backend_not_ready_on_503() {
        let hp =
            spawn_stub_backend("HTTP/1.1 503 Service Unavailable\r\nContent-Length: 0\r\n\r\n");
        assert_eq!(probe_backend(&hp), BackendState::NotReady);
    }

    #[test]
    fn test_backend_http10_and_scheme_path_url() {
        // Bare host:port, scheme-only, and scheme+path forms all probe equally.
        // (Each stub answers exactly one connection, so spawn one per form.)
        let hp = spawn_stub_backend("HTTP/1.0 200 OK\r\nContent-Length: 0\r\n\r\n");
        assert_eq!(probe_backend(&hp), BackendState::Up);
        let hp = spawn_stub_backend("HTTP/1.0 200 OK\r\nContent-Length: 0\r\n\r\n");
        assert_eq!(probe_backend(&format!("http://{hp}")), BackendState::Up);
        let hp = spawn_stub_backend("HTTP/1.1 200 OK\r\nContent-Length: 0\r\n\r\n");
        assert_eq!(
            probe_backend(&format!("http://{hp}/api/ready")),
            BackendState::Up
        );
    }

    #[test]
    fn test_backend_up_on_slow_or_fragmented_response() {
        // First byte delayed 300ms (loaded event loop) — still within budget.
        let hp = spawn_custom_backend(|mut stream| {
            use std::io::Write;
            std::thread::sleep(Duration::from_millis(300));
            let _ = stream.write_all(b"HTTP/1.1 200 OK\r\n\r\n");
        });
        assert_eq!(probe_backend(&hp), BackendState::Up);

        // Status line dribbled out byte by byte — must be reassembled.
        let hp = spawn_custom_backend(|mut stream| {
            use std::io::Write;
            for b in b"HTTP/1.1 404 Not Found\r\n\r\n" {
                let _ = stream.write_all(&[*b]);
                std::thread::sleep(Duration::from_millis(5));
            }
        });
        assert_eq!(probe_backend(&hp), BackendState::Up);
    }

    #[test]
    fn test_backend_down_on_silent_garbage_or_refused() {
        // Accepts but never answers within the probe budget → down.
        let hp = spawn_custom_backend(|stream| {
            std::thread::sleep(Duration::from_millis(2500));
            drop(stream);
        });
        assert_eq!(probe_backend(&hp), BackendState::Down);

        let hp = spawn_stub_backend("this is not http");
        assert_eq!(probe_backend(&hp), BackendState::Down);

        // Nothing listening on this port (bind+drop leaves it closed).
        let port = std::net::TcpListener::bind("127.0.0.1:0")
            .unwrap()
            .local_addr()
            .unwrap()
            .port();
        assert_eq!(
            probe_backend(&format!("127.0.0.1:{port}")),
            BackendState::Down
        );
        assert_eq!(probe_backend("not-a-socket-addr"), BackendState::Down);
    }

    #[test]
    fn test_detect_zh() {
        let mut headers = header::HeaderMap::new();
        headers.insert("accept-language", "zh-CN,zh;q=0.9".parse().unwrap());
        assert_eq!(detect_language(&headers), "zh-Hans");
    }

    #[test]
    fn test_detect_zht() {
        let mut headers = header::HeaderMap::new();
        headers.insert("accept-language", "zh-TW,zh;q=0.9".parse().unwrap());
        assert_eq!(detect_language(&headers), "zh-Hant");
    }

    #[test]
    fn test_detect_ja() {
        let mut headers = header::HeaderMap::new();
        headers.insert("accept-language", "ja".parse().unwrap());
        assert_eq!(detect_language(&headers), "ja");
    }

    #[test]
    fn test_detect_unknown() {
        let mut headers = header::HeaderMap::new();
        headers.insert("accept-language", "xx-XX".parse().unwrap());
        assert_eq!(detect_language(&headers), "en");
    }

    #[test]
    fn test_all_langs_have_data() {
        // The SPA renders this catalogue verbatim (it carries no copy table of
        // its own any more), so a key present in only some languages would
        // surface as an English fragment inside a translated page.
        let english: Vec<&String> = {
            let mut keys: Vec<&String> = get_i18n("en").keys().collect();
            keys.sort();
            keys
        };
        assert!(
            !english.is_empty(),
            "the English catalogue must not be empty"
        );
        for lang in SUPPORTED {
            let i18n = get_i18n(lang);
            let mut keys: Vec<&String> = i18n.keys().collect();
            keys.sort();
            assert_eq!(
                keys, english,
                "{lang} must carry exactly the English key set (missing or extra keys)"
            );
            for required in ["title", "heading", "tagline", "footer", "binaries_title"] {
                assert!(
                    i18n.contains_key(required),
                    "Missing '{required}' for language: {lang}"
                );
            }
        }
    }

    /// The malkuth nonce cookie must be appended alongside — never replace —
    /// any Set-Cookie headers the backend itself returns.
    #[tokio::test]
    async fn test_proxy_preserves_backend_cookies() {
        let backend = spawn_custom_backend(|mut stream| {
            use std::io::Write;
            let _ = stream.write_all(
                b"HTTP/1.1 200 OK\r\nContent-Length: 2\r\nSet-Cookie: session=abc; Path=/\r\nSet-Cookie: theme=dark; Path=/\r\n\r\nok",
            );
        });
        // Give the stub a moment to start listening.
        tokio::time::sleep(Duration::from_millis(50)).await;

        let req = Request::builder()
            .uri("/")
            .body(axum::body::Body::empty())
            .unwrap();
        let resp = proxy_to_backend(req, &format!("http://{backend}"), "1")
            .await
            .expect("proxy should succeed");

        let cookies: Vec<_> = resp.headers().get_all(header::SET_COOKIE).iter().collect();
        assert_eq!(cookies.len(), 3, "nonce + both backend cookies expected");
        let joined: String = cookies
            .iter()
            .filter_map(|v| v.to_str().ok())
            .collect::<Vec<_>>()
            .join("|");
        assert!(joined.contains("__malkuth_nonce=1"), "nonce cookie missing");
        assert!(joined.contains("session=abc"), "backend cookie 1 missing");
        assert!(joined.contains("theme=dark"), "backend cookie 2 missing");
    }

    /// Only 502/503/504 (backend unavailable / draining) may be masked with
    /// the landing page; every other status is a live backend answer.
    #[test]
    fn test_should_fall_back_classification() {
        for status in [200u16, 204, 301, 302, 304, 400, 404, 418, 500] {
            assert!(
                !should_fall_back(status),
                "status {status} must pass through to the browser"
            );
        }
        for status in [502u16, 503, 504] {
            assert!(
                should_fall_back(status),
                "status {status} must fall back to the landing page"
            );
        }
    }

    /// Backend 4xx/5xx answers (other than 502/503/504) are live responses
    /// and must reach the browser untouched — status and body — instead of
    /// being masked as "backend offline".
    #[tokio::test]
    async fn test_proxy_passes_through_error_statuses() {
        for (status_line, expected) in [
            ("404 Not Found", StatusCode::NOT_FOUND),
            (
                "500 Internal Server Error",
                StatusCode::INTERNAL_SERVER_ERROR,
            ),
        ] {
            let backend = spawn_stub_backend(Box::leak(
                format!("HTTP/1.1 {status_line}\r\nContent-Length: 2\r\nX-Marker: stub\r\n\r\nok")
                    .into_boxed_str(),
            ));
            // Give the stub a moment to start listening.
            tokio::time::sleep(Duration::from_millis(50)).await;

            let req = Request::builder()
                .uri("/")
                .body(axum::body::Body::empty())
                .unwrap();
            let resp = proxy_to_backend(req, &format!("http://{backend}"), "1")
                .await
                .unwrap_or_else(|_| panic!("status `{status_line}` must pass through"));

            assert_eq!(resp.status(), expected);
            assert_eq!(resp.headers().get("x-marker").unwrap(), "stub");
            let body = axum::body::to_bytes(resp.into_body(), usize::MAX)
                .await
                .unwrap();
            assert_eq!(&body[..], b"ok");
        }
    }

    /// Redirects must reach the browser untouched: the proxy must not follow
    /// them itself (following an absolute redirect could loop back through
    /// the proxy and surface the landing page as a bogus 200). The stub is
    /// one-shot, so a followed redirect would fail the request outright.
    #[tokio::test]
    async fn test_proxy_passes_through_redirect_without_following() {
        let backend = spawn_stub_backend(
            "HTTP/1.1 301 Moved Permanently\r\nContent-Length: 0\r\nLocation: http://example.com/new\r\n\r\n",
        );
        // Give the stub a moment to start listening.
        tokio::time::sleep(Duration::from_millis(50)).await;

        let req = Request::builder()
            .uri("/")
            .body(axum::body::Body::empty())
            .unwrap();
        let resp = proxy_to_backend(req, &format!("http://{backend}"), "1")
            .await
            .expect("301 must pass through instead of being followed");

        assert_eq!(resp.status(), StatusCode::MOVED_PERMANENTLY);
        assert_eq!(
            resp.headers().get(header::LOCATION).unwrap(),
            "http://example.com/new"
        );
    }

    /// 502/503/504 are gateway semantics for "backend unavailable /
    /// draining" (mirroring the probe's 503 → NotReady verdict) and still
    /// fall back to the landing page.
    #[tokio::test]
    async fn test_proxy_falls_back_on_gateway_statuses() {
        for status_line in [
            "502 Bad Gateway",
            "503 Service Unavailable",
            "504 Gateway Timeout",
        ] {
            let backend = spawn_stub_backend(Box::leak(
                format!("HTTP/1.1 {status_line}\r\nContent-Length: 0\r\n\r\n").into_boxed_str(),
            ));
            // Give the stub a moment to start listening.
            tokio::time::sleep(Duration::from_millis(50)).await;

            let req = Request::builder()
                .uri("/")
                .body(axum::body::Body::empty())
                .unwrap();
            assert!(
                proxy_to_backend(req, &format!("http://{backend}"), "1")
                    .await
                    .is_err(),
                "status `{status_line}` must fall back to the landing page"
            );
        }
    }

    #[test]
    fn test_is_upgrade_request_classification() {
        fn headers(pairs: &[(&str, &str)]) -> header::HeaderMap {
            let mut map = header::HeaderMap::new();
            for (name, value) in pairs {
                map.insert(
                    header::HeaderName::from_bytes(name.as_bytes()).unwrap(),
                    value.parse().unwrap(),
                );
            }
            map
        }

        // Canonical WebSocket handshake.
        assert!(is_upgrade_request(&headers(&[
            ("connection", "upgrade"),
            ("upgrade", "websocket"),
        ])));
        // Token-list Connection and mixed casing must match.
        assert!(is_upgrade_request(&headers(&[
            ("Connection", "keep-alive, UpGrAdE"),
            ("Upgrade", "WebSocket"),
        ])));
        // Any Upgrade value qualifies (generic upgrade semantics).
        assert!(is_upgrade_request(&headers(&[
            ("connection", "Upgrade"),
            ("upgrade", "h2c"),
        ])));
        // An Upgrade header alone is not an upgrade request...
        assert!(!is_upgrade_request(&headers(&[("upgrade", "websocket")])));
        // ...nor is `Connection: upgrade` without an `Upgrade` header...
        assert!(!is_upgrade_request(&headers(&[("connection", "upgrade")])));
        // ...nor plain keep-alive traffic.
        assert!(!is_upgrade_request(&headers(&[(
            "connection",
            "keep-alive"
        )])));
        assert!(!is_upgrade_request(&headers(&[("connection", "close")])));
        assert!(!is_upgrade_request(&headers(&[])));
    }

    #[test]
    fn test_backend_host_port_extraction() {
        assert_eq!(backend_host_port("127.0.0.1:8080"), Some("127.0.0.1:8080"));
        assert_eq!(
            backend_host_port("http://127.0.0.1:8080"),
            Some("127.0.0.1:8080")
        );
        assert_eq!(
            backend_host_port("http://127.0.0.1:8080/api/ws"),
            Some("127.0.0.1:8080")
        );
        assert_eq!(
            backend_host_port("https://example.com:9443"),
            Some("example.com:9443")
        );
        assert_eq!(backend_host_port(""), None);
        assert_eq!(backend_host_port("http://"), None);
    }

    #[test]
    fn test_parse_response_head() {
        let (status, headers) = parse_response_head(
            b"HTTP/1.1 101 Switching Protocols\r\nUpgrade: websocket\r\nSec-WebSocket-Accept: abc\r\nConnection: Upgrade\r\n\r\n",
        )
        .unwrap();
        assert_eq!(status, 101);
        assert!(headers.contains(&("upgrade".to_string(), b"websocket".to_vec())));
        assert!(headers.contains(&("sec-websocket-accept".to_string(), b"abc".to_vec())));

        let (status, headers) =
            parse_response_head(b"HTTP/1.1 404 Not Found\r\nContent-Length: 2\r\n\r\n").unwrap();
        assert_eq!(status, 404);
        assert_eq!(
            headers
                .iter()
                .find(|(n, _)| n == "content-length")
                .map(|(_, v)| v.as_slice()),
            Some(b"2".as_slice())
        );

        // Garbage must not parse.
        assert!(parse_response_head(b"not http at all").is_none());
    }

    /// A raw backend answering a real upgrade handshake: the 101 response
    /// must be forwarded verbatim, and the forwarded wire request must carry
    /// the rewrite rules (Host reset, `Connection: Upgrade` re-added).
    #[tokio::test]
    async fn test_proxy_upgrade_tunnels_101_with_headers() {
        let listener = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
        let port = listener.local_addr().unwrap().port();
        std::thread::spawn(move || {
            use std::io::{Read, Write};
            if let Ok((mut stream, _)) = listener.accept() {
                let mut buf = [0u8; 2048];
                let n = stream.read(&mut buf).unwrap_or(0);
                let req_text = String::from_utf8_lossy(&buf[..n]);
                let lowered = req_text.to_ascii_lowercase();
                if !lowered.contains("upgrade: websocket")
                    || !lowered.contains("connection: upgrade")
                    || !lowered.contains("host: 127.0.0.1:")
                {
                    stream
                        .write_all(b"HTTP/1.1 500 Broken Proxy\r\nContent-Length: 0\r\n\r\n")
                        .ok();
                    return;
                }
                let key = req_text
                    .lines()
                    .find(|l| l.to_ascii_lowercase().starts_with("sec-websocket-key"))
                    .and_then(|l| l.split_once(':').map(|(_, v)| v.trim().to_string()))
                    .unwrap_or_default();
                let accept = ws_accept_for_test(&key);
                let resp = format!(
                    "HTTP/1.1 101 Switching Protocols\r\nUpgrade: websocket\r\nConnection: Upgrade\r\nSec-WebSocket-Accept: {accept}\r\nX-Backend: ws\r\n\r\n"
                );
                stream.write_all(resp.as_bytes()).ok();
                // Give the proxy time to finish reading the head before the
                // stub connection disappears.
                std::thread::sleep(Duration::from_millis(200));
            }
        });
        tokio::time::sleep(Duration::from_millis(50)).await;

        let mut req = Request::builder()
            .uri("/api/rpc?workspace=test")
            .header("host", "front.door.example")
            .header("connection", "upgrade")
            .header("upgrade", "websocket")
            .header("sec-websocket-key", "dGhlIHNhbXBsZSBub25jZQ==")
            .header("sec-websocket-version", "13")
            .body(axum::body::Body::empty())
            .unwrap();
        // A real hyper server would attach a live OnUpgrade; a none()-style
        // one exercises the same decision path (the detached tunnel simply
        // fails to upgrade and exits).
        let on_upgrade = hyper::upgrade::on(&mut req);
        req.extensions_mut().insert(on_upgrade);

        let resp = proxy_upgrade(req, &format!("http://127.0.0.1:{port}"), "1")
            .await
            .expect("101 must pass through");
        assert_eq!(resp.status(), StatusCode::SWITCHING_PROTOCOLS);
        assert_eq!(resp.headers().get("upgrade").unwrap(), "websocket");
        assert_eq!(resp.headers().get("connection").unwrap(), "Upgrade");
        assert_eq!(
            resp.headers()
                .get("sec-websocket-accept")
                .unwrap()
                .to_str()
                .unwrap(),
            ws_accept_for_test("dGhlIHNhbXBsZSBub25jZQ==")
        );
        assert_eq!(resp.headers().get("x-backend").unwrap(), "ws");
    }

    /// A non-101 backend answer to an upgrade handshake passes through
    /// untouched (#92 semantics), with the nonce cookie appended alongside
    /// any backend Set-Cookie.
    #[tokio::test]
    async fn test_proxy_upgrade_passes_through_non_101() {
        let backend = spawn_stub_backend(
            "HTTP/1.1 404 Not Found\r\nContent-Length: 2\r\nX-Marker: ws404\r\nSet-Cookie: session=abc; Path=/\r\n\r\nno",
        );
        tokio::time::sleep(Duration::from_millis(50)).await;

        let mut req = Request::builder()
            .uri("/missing")
            .header("connection", "upgrade")
            .header("upgrade", "websocket")
            .body(axum::body::Body::empty())
            .unwrap();
        let on_upgrade = hyper::upgrade::on(&mut req);
        req.extensions_mut().insert(on_upgrade);

        let resp = proxy_upgrade(req, &format!("http://{backend}"), "1")
            .await
            .expect("404 must pass through instead of being masked");
        assert_eq!(resp.status(), StatusCode::NOT_FOUND);
        assert_eq!(resp.headers().get("x-marker").unwrap(), "ws404");
        let cookies: Vec<_> = resp.headers().get_all(header::SET_COOKIE).iter().collect();
        let joined: String = cookies
            .iter()
            .filter_map(|v| v.to_str().ok())
            .collect::<Vec<_>>()
            .join("|");
        assert!(joined.contains("__malkuth_nonce=1"), "nonce cookie missing");
        assert!(joined.contains("session=abc"), "backend cookie missing");
        let body = axum::body::to_bytes(resp.into_body(), usize::MAX)
            .await
            .unwrap();
        assert_eq!(&body[..], b"no");
    }

    /// Chunked non-101 bodies are decoded before being forwarded.
    #[tokio::test]
    async fn test_proxy_upgrade_decodes_chunked_body() {
        let backend = spawn_stub_backend(
            "HTTP/1.1 200 OK\r\nTransfer-Encoding: chunked\r\n\r\n4\r\nWiki\r\n5\r\npedia\r\n0\r\n\r\n",
        );
        tokio::time::sleep(Duration::from_millis(50)).await;

        let mut req = Request::builder()
            .uri("/ws")
            .header("connection", "upgrade")
            .header("upgrade", "websocket")
            .body(axum::body::Body::empty())
            .unwrap();
        let on_upgrade = hyper::upgrade::on(&mut req);
        req.extensions_mut().insert(on_upgrade);

        let resp = proxy_upgrade(req, &format!("http://{backend}"), "1")
            .await
            .expect("chunked 200 must pass through");
        assert_eq!(resp.status(), StatusCode::OK);
        let body = axum::body::to_bytes(resp.into_body(), usize::MAX)
            .await
            .unwrap();
        assert_eq!(&body[..], b"Wikipedia");
    }

    /// Transport failures and 502/503/504 answers still fall back to the
    /// landing page on the upgrade path.
    #[tokio::test]
    async fn test_proxy_upgrade_falls_back_on_transport_and_gateway() {
        // Nothing listening on this port (bind+drop leaves it closed).
        let port = std::net::TcpListener::bind("127.0.0.1:0")
            .unwrap()
            .local_addr()
            .unwrap()
            .port();
        let mut req = Request::builder()
            .uri("/ws")
            .header("connection", "upgrade")
            .header("upgrade", "websocket")
            .body(axum::body::Body::empty())
            .unwrap();
        let on_upgrade = hyper::upgrade::on(&mut req);
        req.extensions_mut().insert(on_upgrade);
        assert!(
            proxy_upgrade(req, &format!("http://127.0.0.1:{port}"), "1")
                .await
                .is_err(),
            "unreachable backend must fall back to the landing page"
        );

        for status_line in [
            "502 Bad Gateway",
            "503 Service Unavailable",
            "504 Gateway Timeout",
        ] {
            let backend = spawn_stub_backend(Box::leak(
                format!("HTTP/1.1 {status_line}\r\nContent-Length: 0\r\n\r\n").into_boxed_str(),
            ));
            tokio::time::sleep(Duration::from_millis(50)).await;
            let mut req = Request::builder()
                .uri("/ws")
                .header("connection", "upgrade")
                .header("upgrade", "websocket")
                .body(axum::body::Body::empty())
                .unwrap();
            let on_upgrade = hyper::upgrade::on(&mut req);
            req.extensions_mut().insert(on_upgrade);
            assert!(
                proxy_upgrade(req, &format!("http://{backend}"), "1")
                    .await
                    .is_err(),
                "status `{status_line}` must fall back to the landing page"
            );
        }
    }

    #[test]
    fn test_ws_accept_derivation() {
        // RFC 6455 §1.3 example key.
        assert_eq!(
            ws_accept_for_test("dGhlIHNhbXBsZSBub25jZQ=="),
            "s3pPLMBiTxaQ9kYGzzhZRbK+xOo="
        );
    }

    /// base64(sha1(key + RFC 6455 GUID)) — the canonical WebSocket
    /// Sec-WebSocket-Accept derivation (mirrors examples/test_app).
    fn ws_accept_for_test(key: &str) -> String {
        const GUID: &[u8] = b"258EAFA5-E914-47DA-95CA-C5AB0DC85B11";
        let mut input = key.as_bytes().to_vec();
        input.extend_from_slice(GUID);
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

    // ── Build-epoch landing interstitial ─────────────────────────────

    #[test]
    fn test_probe_backend_epoch_hashes_html() {
        let html = "<html><body>build A</body></html>";
        let hp = spawn_stub_backend(Box::leak(
            format!("HTTP/1.1 200 OK\r\nContent-Type: text/html; charset=utf-8\r\nConnection: close\r\nContent-Length: {}\r\n\r\n{html}", html.len())
                .into_boxed_str(),
        ));
        let first = probe_backend_epoch(&hp).expect("HTML document must yield a token");
        assert_eq!(first.len(), 12, "token is a 12-char hex prefix");
        assert!(first.chars().all(|c| c.is_ascii_hexdigit()));

        // Identical body → identical token (content-addressed).
        let hp = spawn_stub_backend(Box::leak(
            format!("HTTP/1.1 200 OK\r\nContent-Type: text/html; charset=utf-8\r\nConnection: close\r\nContent-Length: {}\r\n\r\n{html}", html.len())
                .into_boxed_str(),
        ));
        assert_eq!(probe_backend_epoch(&hp).as_deref(), Some(first.as_str()));

        // Changed body (a rebuild) → different token.
        let rebuilt = "<html><body>build B with new asset hashes</body></html>";
        let hp = spawn_stub_backend(Box::leak(
            format!("HTTP/1.1 200 OK\r\nContent-Type: text/html; charset=utf-8\r\nConnection: close\r\nContent-Length: {}\r\n\r\n{rebuilt}", rebuilt.len())
                .into_boxed_str(),
        ));
        let second = probe_backend_epoch(&hp).expect("rebuilt HTML must yield a token");
        assert_ne!(first, second, "a rebuild must change the token");
    }

    /// A backend that sends its head plus a partial body and then stalls
    /// (never closes within the read budget) must NOT yield a token: the
    /// partial hash could flip between probe cycles whenever the delivery
    /// time jitters around the budget, re-showing the landing page on
    /// every flip. The previous token must be kept instead (None here).
    #[test]
    fn test_probe_backend_epoch_rejects_partial_body() {
        let hp = spawn_custom_backend(|mut stream| {
            use std::io::Write;
            let _ = stream.write_all(
                b"HTTP/1.1 200 OK\r\nContent-Type: text/html\r\nConnection: close\r\nContent-Length: 100\r\n\r\n<html><body>part",
            );
            // Hold the connection open well past the probe budget without
            // delivering the remaining body bytes.
            std::thread::sleep(Duration::from_millis(3000));
        });
        assert_eq!(
            probe_backend_epoch(&hp),
            None,
            "a stalled partial body must not be hashed into a build token"
        );
    }

    #[test]
    fn test_probe_backend_epoch_rejects_non_html() {
        // JSON answer (API-only backend) → no token.
        let hp = spawn_stub_backend(
            "HTTP/1.1 200 OK\r\nContent-Type: application/json\r\nConnection: close\r\nContent-Length: 2\r\n\r\n{}",
        );
        assert_eq!(probe_backend_epoch(&hp), None);

        // Non-200 status → no token.
        let hp = spawn_stub_backend(
            "HTTP/1.1 404 Not Found\r\nContent-Type: text/html\r\nConnection: close\r\nContent-Length: 0\r\n\r\n",
        );
        assert_eq!(probe_backend_epoch(&hp), None);

        // Nothing listening → no token.
        let port = std::net::TcpListener::bind("127.0.0.1:0")
            .unwrap()
            .local_addr()
            .unwrap()
            .port();
        assert_eq!(probe_backend_epoch(&format!("127.0.0.1:{port}")), None);
    }

    #[test]
    fn test_read_nonce_raw_value() {
        let req = Request::builder()
            .header(
                header::COOKIE,
                "theme=dark; __malkuth_nonce=deadbeefcafe; other=1",
            )
            .body(axum::body::Body::empty())
            .unwrap();
        assert_eq!(read_nonce(&req), "deadbeefcafe");

        let req = Request::builder()
            .header(header::COOKIE, "theme=dark")
            .body(axum::body::Body::empty())
            .unwrap();
        assert_eq!(read_nonce(&req), "");

        let req = Request::builder().body(axum::body::Body::empty()).unwrap();
        assert_eq!(read_nonce(&req), "");
    }

    #[test]
    fn test_request_interceptable_classification() {
        let build = |method: &str, hdrs: &[(&str, &str)]| -> Request {
            let mut b = Request::builder().method(method).uri("/");
            for (n, v) in hdrs {
                b = b.header(*n, *v);
            }
            b.body(axum::body::Body::empty()).unwrap()
        };

        // Document loads are intercepted (browser navigation).
        assert!(request_interceptable(&build(
            "GET",
            &[("accept", "text/html,application/xhtml+xml,*/*;q=0.8")]
        )));
        assert!(request_interceptable(&build(
            "GET",
            &[("sec-fetch-dest", "document")]
        )));
        assert!(request_interceptable(&build(
            "GET",
            &[("sec-fetch-dest", "iframe"), ("accept", "*/*")]
        )));

        // API / XHR / machine traffic is forwarded untouched.
        assert!(!request_interceptable(&build(
            "GET",
            &[("accept", "application/json")]
        )));
        assert!(!request_interceptable(&build("GET", &[("accept", "*/*")])));
        assert!(!request_interceptable(&build(
            "GET",
            &[("sec-fetch-dest", "empty"), ("accept", "text/html")]
        )));
        assert!(!request_interceptable(&build(
            "POST",
            &[("accept", "text/html")]
        )));
    }

    /// A persistent stub backend: answers any number of connections,
    /// serving `/` with a swappable HTML document (to simulate a
    /// rebuild), `/api/data` with JSON, and `/readyz` with 200.
    struct RebuildableBackend {
        port: u16,
        doc: std::sync::Arc<std::sync::Mutex<String>>,
    }

    impl RebuildableBackend {
        fn start() -> Self {
            let listener = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
            let port = listener.local_addr().unwrap().port();
            let doc = std::sync::Arc::new(std::sync::Mutex::new(
                "<html><body>app build one</body></html>".to_string(),
            ));
            let doc2 = std::sync::Arc::clone(&doc);
            std::thread::spawn(move || {
                for stream in listener.incoming() {
                    let Ok(mut stream) = stream else { continue };
                    let doc = std::sync::Arc::clone(&doc2);
                    std::thread::spawn(move || {
                        use std::io::{Read, Write};
                        let mut buf = [0u8; 2048];
                        let _ = stream.read(&mut buf);
                        let head = String::from_utf8_lossy(&buf);
                        let path = head
                            .split_whitespace()
                            .nth(1)
                            .unwrap_or("/")
                            .split('?')
                            .next()
                            .unwrap_or("/")
                            .to_string();
                        let (ctype, body) = match path.as_str() {
                            "/readyz" => ("text/plain", "ok".to_string()),
                            "/api/data" => ("application/json", r#"{"data":42}"#.to_string()),
                            _ => ("text/html; charset=utf-8", doc.lock().unwrap().clone()),
                        };
                        let resp = format!(
                            "HTTP/1.1 200 OK\r\nContent-Type: {ctype}\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{body}",
                            body.len()
                        );
                        let _ = stream.write_all(resp.as_bytes());
                    });
                }
            });
            Self { port, doc }
        }

        fn url(&self) -> String {
            format!("http://127.0.0.1:{}", self.port)
        }

        /// Swap the served document — a "rebuild".
        fn rebuild(&self, html: &str) {
            *self.doc.lock().unwrap() = html.to_string();
        }
    }

    /// End-to-end serve-door behaviour, driven through the real router:
    /// first document visit → landing; API traffic without a token →
    /// forwarded (and stamped with the build token); document visit with
    /// the current token → forwarded; after a rebuild the old token no
    /// longer unlocks the document, while API traffic keeps flowing.
    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn test_serve_door_landing_per_build() {
        let backend = RebuildableBackend::start();

        let router = info_router(
            "test".to_string(),
            InfoStatus::Landing,
            vec![],
            None,
            vec![],
            Some(backend.url()),
            vec![],
            std::sync::Arc::new(std::sync::Mutex::new(None)),
            std::sync::Arc::new(std::sync::Mutex::new(vec![])),
            std::sync::Arc::new(std::sync::Mutex::new(vec![])),
        );
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let door = listener.local_addr().unwrap();
        tokio::spawn(async move {
            let _ = axum::serve(listener, router).await;
        });

        // Machine client for the door: no env proxies, no cookie store,
        // no redirect following — the nonce cookie is driven explicitly.
        let client = crate::http_client_builder()
            .no_proxy()
            .redirect(reqwest::redirect::Policy::none())
            .build()
            .unwrap();
        let door_url = format!("http://{door}");

        // 1. First document visit (browser navigation): landing page, not
        //    the app. Must hold for both the real SPA landing and the
        //    minimal stub fallback.
        let resp = client
            .get(format!("{door_url}/"))
            .header("accept", "text/html,application/xhtml+xml,*/*;q=0.8")
            .send()
            .await
            .unwrap();
        assert_eq!(resp.status(), StatusCode::OK);
        assert!(
            resp.headers()
                .get(header::CONTENT_TYPE)
                .is_some_and(|v| v.as_bytes().starts_with(b"text/html"))
        );
        let body = resp.text().await.unwrap();
        assert!(
            !body.contains("app build one"),
            "first document visit must see the landing page, not the backend document"
        );

        // 2. API traffic without a token: forwarded, and the response
        //    stamps the current build token.
        let resp = client
            .get(format!("{door_url}/api/data"))
            .header("accept", "application/json")
            .send()
            .await
            .unwrap();
        assert_eq!(resp.status(), StatusCode::OK);
        let stamp = resp
            .headers()
            .get(header::SET_COOKIE)
            .and_then(|v| v.to_str().ok())
            .expect("proxied response must stamp the build token")
            .to_string();
        let epoch = stamp
            .strip_prefix("__malkuth_nonce=")
            .and_then(|rest| rest.split(';').next())
            .expect("stamp must be the __malkuth_nonce cookie")
            .to_string();
        assert_eq!(epoch.len(), 12, "token must be the hashed document");
        let body = resp.text().await.unwrap();
        assert_eq!(body, "{\"data\":42}", "API traffic must reach the backend");

        // 3. Document visit with the current token: forwarded to the app.
        let resp = client
            .get(format!("{door_url}/"))
            .header("accept", "text/html")
            .header(header::COOKIE, format!("__malkuth_nonce={epoch}"))
            .send()
            .await
            .unwrap();
        let body = resp.text().await.unwrap();
        assert!(
            body.contains("app build one"),
            "a matching token must unlock the backend document"
        );

        // 4. Rebuild: wait for the probe loop to pick up the new document
        //    (poll the stamped token on the API path), then verify the old
        //    token re-locks the document while API traffic keeps flowing.
        backend.rebuild("<html><body>app build two</body></html>");
        let mut new_epoch = None;
        for _ in 0..80 {
            tokio::time::sleep(Duration::from_millis(250)).await;
            let resp = client
                .get(format!("{door_url}/api/data"))
                .header("accept", "application/json")
                .send()
                .await
                .unwrap();
            let stamp = resp
                .headers()
                .get(header::SET_COOKIE)
                .and_then(|v| v.to_str().ok())
                .unwrap_or("")
                .to_string();
            if let Some(e) = stamp
                .strip_prefix("__malkuth_nonce=")
                .and_then(|rest| rest.split(';').next())
            {
                if e != epoch {
                    new_epoch = Some(e.to_string());
                    break;
                }
            }
        }
        let new_epoch = new_epoch.expect("probe loop must observe the rebuilt document token");

        // Old token: landing again (the once-per-build interstitial).
        let resp = client
            .get(format!("{door_url}/"))
            .header("accept", "text/html")
            .header(header::COOKIE, format!("__malkuth_nonce={epoch}"))
            .send()
            .await
            .unwrap();
        let body = resp.text().await.unwrap();
        assert!(
            !body.contains("app build"),
            "a stale token must re-show the landing page after a rebuild"
        );

        // New token: the rebuilt document.
        let resp = client
            .get(format!("{door_url}/"))
            .header("accept", "text/html")
            .header(header::COOKIE, format!("__malkuth_nonce={new_epoch}"))
            .send()
            .await
            .unwrap();
        let body = resp.text().await.unwrap();
        assert!(
            body.contains("app build two"),
            "the new token must unlock the rebuilt document"
        );

        // Old-token API traffic still flows (previous-build sessions keep
        // working until their next document navigation).
        let resp = client
            .get(format!("{door_url}/api/data"))
            .header("accept", "application/json")
            .header(header::COOKIE, format!("__malkuth_nonce={epoch}"))
            .send()
            .await
            .unwrap();
        let body = resp.text().await.unwrap();
        assert_eq!(body, "{\"data\":42}");
    }

    /// The JSON probe and the SPA init must tell the landing page whether
    /// its origin can actually be proxied into the backend, so the page
    /// never enters a poll-driven reload loop on a door that can only
    /// re-render itself. `serve` is true only for a `--serve` door whose
    /// `--serve-host` allowlist admits the request's Host; false for
    /// host mismatches and for plain info doors without `--serve`.
    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn test_probe_reports_serve_capability() {
        // Serve a router on a pre-bound listener. `serve_hosts_mode`:
        // "empty" (no restriction), "own" (restrict to this door's own
        // host:port — what reqwest sends as Host by default), or
        // "foreign" (restrict to an unrelated host, so this origin is
        // never proxied).
        async fn spawn_router(
            serve_backend: Option<String>,
            serve_hosts_mode: &str,
        ) -> (reqwest::Client, String) {
            // Bind first so the door's own host:port is known before the
            // router (and its --serve-host allowlist) is constructed.
            let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
            let door = listener.local_addr().unwrap();
            let serve_hosts = match serve_hosts_mode {
                "own" => vec![door.to_string()],
                "foreign" => vec!["other.example".to_string()],
                _ => vec![],
            };
            let router = info_router(
                "test".to_string(),
                InfoStatus::Landing,
                vec![],
                None,
                vec![],
                serve_backend,
                serve_hosts,
                std::sync::Arc::new(std::sync::Mutex::new(None)),
                std::sync::Arc::new(std::sync::Mutex::new(vec![])),
                std::sync::Arc::new(std::sync::Mutex::new(vec![])),
            );
            tokio::spawn(async move {
                let _ = axum::serve(listener, router).await;
            });
            let client = crate::http_client_builder()
                .no_proxy()
                .redirect(reqwest::redirect::Policy::none())
                .build()
                .unwrap();
            (client, format!("http://{door}"))
        }

        async fn probe_serve(client: &reqwest::Client, door_url: &str) -> bool {
            let body = client
                .get(format!("{door_url}/"))
                .header("x-malkuth-probe", "1")
                .send()
                .await
                .unwrap()
                .text()
                .await
                .unwrap();
            let json: serde_json::Value = serde_json::from_str(&body).unwrap();
            json.get("serve")
                .and_then(|v| v.as_bool())
                .expect("probe JSON must carry a boolean serve capability")
        }

        let backend = RebuildableBackend::start();

        // 1. Plain info door (no --serve): never proxied.
        let (client, door_url) = spawn_router(None, "empty").await;
        assert!(
            !probe_serve(&client, &door_url).await,
            "a door without --serve must report serve=false"
        );

        // 2. Serve door without host restrictions: proxied for any Host.
        let (client, door_url) = spawn_router(Some(backend.url()), "empty").await;
        assert!(
            probe_serve(&client, &door_url).await,
            "an unrestricted --serve door must report serve=true"
        );

        // 3. Serve door restricted to this door's own Host (what reqwest
        //    sends by default): the probe must agree with the document
        //    path — allowed.
        let (client, door_url) = spawn_router(Some(backend.url()), "own").await;
        assert!(
            probe_serve(&client, &door_url).await,
            "a --serve-host matching the request Host must report serve=true"
        );

        // The SPA init must agree with the probe for the same origin.
        let body = client
            .get(format!("{door_url}/"))
            .header("accept", "text/html")
            .send()
            .await
            .unwrap()
            .text()
            .await
            .unwrap();
        if let Some(start) = body.find("__MALKUTH_INIT__ = ") {
            let rest = &body[start + "__MALKUTH_INIT__ = ".len()..];
            let end = rest.find("</script>").expect("init script must terminate");
            let json_str = rest[..end].trim().trim_end_matches(';');
            let json: serde_json::Value =
                serde_json::from_str(json_str).expect("init must be valid JSON");
            assert_eq!(
                json.get("serve").and_then(|v| v.as_bool()),
                Some(true),
                "the SPA init must report serve=true on an allowed origin"
            );
        }

        // 4. Serve door restricted to a different Host: this origin is not
        //    proxied — the probe must say so, or a landing page polling
        //    "ready" would reload itself in a loop.
        let (client, door_url) = spawn_router(Some(backend.url()), "foreign").await;
        assert!(
            !probe_serve(&client, &door_url).await,
            "a --serve-host mismatch must report serve=false"
        );
    }
    // ── Prerendered fallback page ────────────────────────────────────────

    /// Build an `InfoState` for the fallback renderer without spawning a door.
    fn fallback_state(
        status: InfoStatus,
        serve_backend: Option<&str>,
        backend: BackendState,
    ) -> InfoState {
        InfoState {
            version: "0.6.0".to_string(),
            status,
            watch_paths: vec!["/srv/celestia/demo/chest".to_string()],
            proxy_endpoint: Some("0.0.0.0:3097 → 3009-3009".to_string()),
            binary_programs: Vec::new(),
            serve_backend: serve_backend.map(str::to_string),
            serve_hosts: Vec::new(),
            build_progress: std::sync::Arc::new(std::sync::Mutex::new(None)),
            build_log: std::sync::Arc::new(std::sync::Mutex::new(Vec::new())),
            runtime_log: std::sync::Arc::new(std::sync::Mutex::new(Vec::new())),
            backend_state: std::sync::Arc::new(tokio::sync::RwLock::new(backend)),
            epoch: std::sync::Arc::new(tokio::sync::RwLock::new("1".to_string())),
        }
    }

    async fn fallback_body(lang: &str, state: &InfoState, allowed: bool) -> String {
        let response = serve_fallback(lang, state, allowed).await;
        let bytes = axum::body::to_bytes(response.into_body(), usize::MAX)
            .await
            .expect("fallback body");
        String::from_utf8(bytes.to_vec()).expect("utf-8")
    }

    /// The fallback renderer, end to end: every placeholder is substituted and
    /// the state/capability axis drives the badge, the action and the
    /// self-refresh (the deleted Tera tests used to pin the last two).
    #[tokio::test]
    async fn test_fallback_page_renders_states_and_honours_serve_capability() {
        // offline: error badge, no countdown, no auto-refresh, manual action.
        let offline = fallback_state(
            InfoStatus::Ready,
            Some("http://127.0.0.1:1"),
            BackendState::Down,
        );
        let html = fallback_body("en", &offline, true).await;
        assert!(
            !html.contains("{{MALKUTH_"),
            "no marker may survive the render"
        );
        assert!(
            !html.contains("http-equiv=\"refresh\""),
            "offline must not self-refresh"
        );
        assert!(
            !html.contains(r#"class="mk-countdown""#),
            "offline renders no countdown (the stylesheet still names the class)"
        );
        assert!(
            // Prefixed like the mk-countdown guard below: the stylesheet
            // names both classes, so only the markup form can prove absence.
            !html.contains(r#"class="mk-retry""#),
            "offline renders no retry paragraph"
        );
        assert!(
            html.contains(r#"class="hk-badge hk-badge-error""#),
            "offline badge"
        );
        assert!(
            html.contains("Refresh Now"),
            "manual action stays available"
        );
        assert!(
            html.contains("/srv/celestia/demo/chest"),
            "watch paths render"
        );

        // ready on a supervisor door: success badge, retry copy at 10s with its
        // unit spliced back in (nothing is being redirected there).
        let ready = fallback_state(InfoStatus::Ready, None, BackendState::Unknown);
        let html = fallback_body("en", &ready, true).await;
        assert!(
            html.contains(r#"class="hk-badge hk-badge-success""#),
            "ready badge"
        );
        assert!(
            html.contains("http-equiv=\"refresh\" content=\"10\""),
            "supervisor ready keeps the 10s retry cadence"
        );
        assert!(
            html.contains("Page will refresh automatically in"),
            "retry copy, not redirect copy"
        );
        assert!(
            !html.contains("Redirecting in"),
            "a supervisor door is not redirecting anywhere"
        );
        assert!(
            html.contains("<strong class=\"mk-countdown\">10</strong><span> seconds</span>"),
            "the retry sentence carries its unit"
        );

        // landing on a proxy door: redirect copy at the 3s beat.
        let landing = fallback_state(
            InfoStatus::Landing,
            Some("http://127.0.0.1:1"),
            BackendState::Up,
        );
        let html = fallback_body("en", &landing, true).await;
        assert!(
            html.contains("http-equiv=\"refresh\" content=\"3\""),
            "landing keeps the 3s redirect cadence"
        );
        assert!(html.contains("Redirecting in"), "redirect copy on landing");

        // a rebuild waits on the slower retry cadence.
        let building = fallback_state(InfoStatus::Working, None, BackendState::Unknown);
        let html = fallback_body("en", &building, true).await;
        assert!(
            html.contains(r#"class="hk-badge hk-badge-warning""#),
            "building badge"
        );
        assert!(
            html.contains("http-equiv=\"refresh\" content=\"10\""),
            "10s retry"
        );

        // S1-02 lock: a door that may NOT be proxied never self-refreshes,
        // even while the backend is up — otherwise it reloads forever.
        let non_proxy = fallback_state(
            InfoStatus::Ready,
            Some("http://127.0.0.1:1"),
            BackendState::Up,
        );
        let html = fallback_body("en", &non_proxy, false).await;
        assert!(
            !html.contains("http-equiv=\"refresh\""),
            "a non-proxy door must not self-refresh"
        );
        assert!(html.contains("Refresh Now"), "manual action still offered");

        // First paint (backend state still Unknown) still redirects, and a
        // non-proxy landing door does not — the loop guard's other half.
        let unknown = fallback_state(
            InfoStatus::Ready,
            Some("http://127.0.0.1:1"),
            BackendState::Unknown,
        );
        let html = fallback_body("en", &unknown, true).await;
        assert!(
            html.contains("http-equiv=\"refresh\" content=\"3\""),
            "Unknown first paint redirects at the 3s beat"
        );
        let html = fallback_body("en", &landing, false).await;
        assert!(
            !html.contains("http-equiv=\"refresh\""),
            "a non-proxy landing door must not self-refresh either"
        );

        // RTL + localisation flow through the same renderer.
        let arabic = fallback_body("ar", &ready, true).await;
        assert!(arabic.contains("lang=\"ar\"") && arabic.contains("dir=\"rtl\""));
    }

    /// `fill` is the only substitution pass: unknown markers survive (so a
    /// renamed marker is caught by the assertion below instead of silently
    /// vanishing) and a value that itself contains a marker is never re-read.
    #[test]
    fn test_fallback_fill_is_single_pass_and_escapes_nothing_implicitly() {
        let template = "<p>{{MALKUTH_A}}</p><p>{{MALKUTH_B}}</p>";
        let out = fill(
            template,
            &[
                ("{{MALKUTH_A}}", "{{MALKUTH_B}}"),
                ("{{MALKUTH_B}}", "done"),
            ],
        );
        assert_eq!(out, "<p>{{MALKUTH_B}}</p><p>done</p>");
        // An unknown marker is preserved verbatim rather than dropped.
        let out = fill("x{{MALKUTH_NOPE}}y", &[]);
        assert_eq!(out, "x{{MALKUTH_NOPE}}y");
    }

    #[test]
    fn test_html_escape_covers_every_attribute_breaker() {
        assert_eq!(
            html_escape(r#"<a href="x">'&'"#),
            "&lt;a href=&quot;x&quot;&gt;&#39;&amp;&#39;"
        );
    }

    /// The init payload is injected into a `<script>` element. Escaping `<`,
    /// `>` and `&` keeps any payload (`</script>`, `<!--`, entity tricks) from
    /// closing the element, while the JSON still round-trips byte-for-byte.
    #[test]
    fn test_init_json_script_escapes_markup_and_round_trips() {
        let payload = serde_json::json!({
            "path": "</script><script>alert(1)</script>",
            "watch": "<!-- & \" ' -->",
            "log": ["</style>"],
        });
        let script = init_json_script(&payload);
        assert!(
            !script.contains("</script"),
            "must not close the script element"
        );
        assert!(!script.contains('<'), "no raw `<` may survive");
        assert!(!script.contains('&'), "no raw `&` may survive");
        assert!(script.contains("\\u003c"), "escaped form used");
        let parsed: serde_json::Value = serde_json::from_str(&script).expect("valid JSON");
        assert_eq!(parsed, payload, "escaping must be lossless");
    }

    /// The embedded fallback page must carry every marker exactly once, and
    /// filling it with sample values must leave no marker behind. This is the
    /// Rust half of the contract build.rs enforces at embed time.
    #[test]
    fn test_fallback_page_markers_are_complete() {
        const MARKERS: [&str; 16] = [
            "{{MALKUTH_LANG}}",
            "{{MALKUTH_DIR}}",
            "{{MALKUTH_TITLE}}",
            "{{MALKUTH_AUTO_REFRESH}}",
            "{{MALKUTH_LOGO_SRC}}",
            "{{MALKUTH_HEADING}}",
            "{{MALKUTH_TAGLINE}}",
            "{{MALKUTH_STATUS_BADGE}}",
            "{{MALKUTH_TASK_ROW}}",
            "{{MALKUTH_PROXY_ROW}}",
            "{{MALKUTH_WATCH_ROW}}",
            "{{MALKUTH_BINARIES_BLOCK}}",
            "{{MALKUTH_RETRY_BLOCK}}",
            "{{MALKUTH_ACTION_BLOCK}}",
            "{{MALKUTH_FOOTER}}",
            "{{MALKUTH_VERSION_LINE}}",
        ];
        for marker in MARKERS {
            assert_eq!(
                FALLBACK_PAGE_HTML.matches(marker).count(),
                1,
                "fallback page must contain {marker} exactly once"
            );
        }
        let slots: Vec<(&str, &str)> = MARKERS.iter().map(|marker| (*marker, "x")).collect();
        let filled = fill(FALLBACK_PAGE_HTML, &slots);
        assert!(
            !filled.contains("{{MALKUTH_"),
            "no marker may survive substitution"
        );
    }

    /// Every catalogue value reaches the page escaped, including the slots
    /// that are interpolated directly into element text. The hostile value
    /// exercises `<`, `&`, quotes and the apostrophe, so a slot that forgets
    /// `text()` fails here instead of shipping malformed markup when a
    /// translator later writes `&` or a quote.
    #[tokio::test]
    async fn test_fallback_page_escapes_hostile_catalogue_values() {
        let state = fallback_state(InfoStatus::Ready, None, BackendState::Unknown);
        let mut i18n: HashMap<String, String> = get_i18n("en").clone();
        let hostile = r#"a&b<c>"d'e<script>alert(1)</script>"#;
        for key in [
            "heading",
            "tagline",
            "footer",
            "title",
            "watch_label",
            "task",
            "task_idle",
            "binaries_title",
            "refresh_label",
            "retry",
            "retry_unit",
            "redirect_before",
            "redirect_after",
            "install_label",
            "proxy_label",
        ] {
            i18n.insert(key.to_string(), hostile.to_string());
        }
        let html = render_fallback_page(&i18n, "en", &state, true).await;
        assert!(
            html.contains("a&amp;b&lt;c&gt;&quot;d&#39;e"),
            "hostile catalogue values must be escaped"
        );
        assert!(!html.contains("<c>"), "no raw tag may survive");
        assert!(
            !html.contains("<script>alert(1)</script>"),
            "no executable markup may survive"
        );
    }

    /// The fallback page's state/badge mapping and the auto-refresh rule.
    #[test]
    fn test_fallback_state_mapping_and_auto_refresh() {
        for (state, key, badge) in [
            ("ready", "status_ready", "success"),
            ("landing", "status_landing", "warning"),
            ("building", "status_building", "warning"),
            ("starting", "status_starting", "warning"),
            ("offline", "status_offline", "error"),
        ] {
            assert_eq!(badge_variant(state), badge, "{state}");
            let i18n = get_i18n("en");
            assert!(
                i18n.contains_key(key),
                "state {state} needs catalogue key {key}"
            );
        }
        assert!(!state_wants_refresh("offline"));
        assert!(!state_wants_refresh("starting"));
        for state in ["ready", "landing", "building"] {
            assert!(state_wants_refresh(state), "{state} should self-retry");
        }
    }
}
