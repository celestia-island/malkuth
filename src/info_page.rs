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

const TEMPLATE: &str = include_str!("info_page/template.html");

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
#[derive(Debug, Clone, serde::Serialize)]
pub struct BinaryInfo {
    pub name: String,
    pub path: String,
    pub compile_time: String,
    pub hash: String,
    #[serde(rename = "hash_short")]
    pub hash_short: String,
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
    binaries: Vec<BinaryInfo>,
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
        binaries,
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
    binaries: Vec<BinaryInfo>,
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
    let (spa_state, message) = if state.serve_backend.is_some() {
        // Serve mode: reflect the real-time backend health so the landing
        // page never promises a redirect while the service is unreachable.
        match *state.backend_state.read().await {
            BackendState::Up | BackendState::Unknown => (
                "landing",
                i18n.get("status_landing")
                    .map_or("Redirecting shortly", |v| v.as_str()),
            ),
            BackendState::NotReady => (
                "starting",
                i18n.get("status_starting")
                    .map_or("The service is starting up", |v| v.as_str()),
            ),
            BackendState::Down => (
                "offline",
                i18n.get("status_offline")
                    .map_or("Service temporarily unavailable", |v| v.as_str()),
            ),
        }
    } else {
        match state.status {
            InfoStatus::Ready => (
                "ready",
                i18n.get("status_ready")
                    .map_or("All services running.", |v| v.as_str()),
            ),
            InfoStatus::Working => (
                "building",
                i18n.get("status_building")
                    .map_or("Building...", |v| v.as_str()),
            ),
            InfoStatus::Landing => (
                "landing",
                i18n.get("status_landing")
                    .map_or("Redirecting shortly", |v| v.as_str()),
            ),
        }
    };
    serde_json::json!({
        "state": spa_state,
        "message": message,
        "binaries": state.binaries,
        "watch_paths": state.watch_paths,
        "proxy_endpoint": state.proxy_endpoint,
        "version": state.version,
        "epoch": state.epoch.read().await.clone(),
        "logo_base64": base64_encode(LOGO_BYTES),
        // Whether a reload of this origin can actually be proxied into the
        // backend (see `info_page`'s `serve_allowed`). The landing page uses
        // it to skip its poll-driven ready-reload on doors that can only
        // re-render themselves, where reloading is a no-op loop.
        "serve": state.serve_backend.is_some() && allowed,
    })
}

async fn serve_spa(state: &InfoState, lang: &str, allowed: bool) -> Response {
    let init_data = build_spa_init(state, lang, allowed).await;
    let init_json = serde_json::to_string(&init_data).unwrap_or_default();
    let init_script = format!("<script>window.__MALKUTH_INIT__ = {};</script>", init_json);
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
    let body_bytes = axum::body::to_bytes(body, usize::MAX)
        .await
        .map_err(|_| ())?;

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

    let client = reqwest::Client::builder()
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

    let body_bytes = axum::body::to_bytes(body, usize::MAX)
        .await
        .map_err(|_| ())?;

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
    let resp_body = resp.bytes().await.map_err(|_| ())?;

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

    let (probe_state, msg) = if state.serve_backend.is_none() || backend_up {
        ("ready", "")
    } else if backend_state == BackendState::Unknown {
        ("landing", "")
    } else if backend_state == BackendState::NotReady {
        ("starting", "")
    } else {
        ("offline", "")
    };

    let message = if msg.is_empty() {
        let i18n = get_i18n(lang);
        match probe_state {
            "landing" => i18n
                .get("status_landing")
                .map_or("Redirecting shortly", |v| v.as_str())
                .to_string(),
            "starting" => i18n
                .get("status_starting")
                .map_or("The service is starting up", |v| v.as_str())
                .to_string(),
            "offline" => i18n
                .get("status_offline")
                .map_or("Service temporarily unavailable", |v| v.as_str())
                .to_string(),
            "ready" => i18n
                .get("status_ready")
                .map_or("All services running.", |v| v.as_str())
                .to_string(),
            _ => msg.to_string(),
        }
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
    let vtty_name = state
        .binaries
        .first()
        .map(|b| b.name.as_str())
        .unwrap_or("");

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

/// Serve the landing page with real-time backend status from the health checker.
async fn serve_landing(lang: &str, state: &InfoState) -> Response {
    let i18n = get_i18n(lang);
    let backend_state = *state.backend_state.read().await;
    let backend_up = state.serve_backend.is_some() && backend_state == BackendState::Up;

    let init_state: &str = if state.serve_backend.is_none() || backend_up {
        "ready"
    } else if backend_state == BackendState::Unknown {
        "landing"
    } else if backend_state == BackendState::NotReady {
        "starting"
    } else {
        "offline"
    };
    let init_msg = match init_state {
        "ready" => i18n
            .get("status_landing")
            .map_or("Redirecting shortly", |v| v.as_str()),
        "offline" => i18n
            .get("status_offline")
            .map_or("Service temporarily unavailable", |v| v.as_str()),
        _ => i18n
            .get("status_starting")
            .map_or("The service is starting up", |v| v.as_str()),
    };

    let mut ctx = tera::Context::new();
    ctx.insert("lang", lang);
    ctx.insert("dir", if lang == "ar" { "rtl" } else { "ltr" });
    ctx.insert("title", i18n.get("title").map_or("Malkuth", |v| v.as_str()));
    ctx.insert(
        "heading",
        i18n.get("heading").map_or("Malkuth", |v| v.as_str()),
    );
    ctx.insert("tagline", i18n.get("tagline").map_or("", |v| v.as_str()));
    ctx.insert("ready", &backend_up);
    ctx.insert(
        "landing",
        &(init_state == "ready" || init_state == "landing"),
    );
    ctx.insert(
        "task",
        i18n.get("task_landing").map_or("Landing", |v| v.as_str()),
    );
    ctx.insert("version", &state.version);
    ctx.insert("status_text", init_msg);
    ctx.insert("initial_state", init_state);
    ctx.insert("malkuth_epoch", &*state.epoch.read().await);
    ctx.insert("task_label", "");
    ctx.insert(
        "redirect_before",
        i18n.get("redirect_before")
            .map_or("Redirecting in", |v| v.as_str()),
    );
    ctx.insert(
        "redirect_after",
        i18n.get("redirect_after").map_or("seconds", |v| v.as_str()),
    );
    ctx.insert(
        "cancel_label",
        i18n.get("cancel_label").map_or("Cancel", |v| v.as_str()),
    );
    ctx.insert(
        "refresh_label",
        i18n.get("refresh_label")
            .map_or("Refresh Now", |v| v.as_str()),
    );
    ctx.insert("retry_before", "");
    ctx.insert("retry_after", "");
    ctx.insert("retry_unit", "");
    ctx.insert("retry_manual", "");
    ctx.insert("footer", "");
    ctx.insert("footer_prefix", "");
    ctx.insert("footer_suffix", "");
    ctx.insert("version_label", "");
    ctx.insert(
        "binaries_title",
        i18n.get("binaries_title")
            .map_or("Supervised Binaries", |v| v.as_str()),
    );
    ctx.insert("binaries", &state.binaries);
    ctx.insert(
        "proxy_endpoint",
        state.proxy_endpoint.as_deref().unwrap_or(""),
    );
    ctx.insert(
        "proxy_label",
        i18n.get("proxy_label").unwrap_or(&"Proxy".to_string()),
    );
    ctx.insert(
        "watch_label",
        i18n.get("watch_label").unwrap_or(&"Watching".to_string()),
    );

    if !state.watch_paths.is_empty() {
        ctx.insert("watch_paths", &state.watch_paths);
    }
    ctx.insert("install_label", "");
    ctx.insert("install_method", "");
    ctx.insert("copy_hint", "");
    ctx.insert("copied_msg", "");
    ctx.insert("copy_fail_msg", "");
    ctx.insert("logo_base64", &base64_encode(LOGO_BYTES));

    match tera::Tera::one_off(TEMPLATE, &ctx, false) {
        Ok(html) => Html(html).into_response(),
        Err(e) => (StatusCode::INTERNAL_SERVER_ERROR, format!("Failed: {e}")).into_response(),
    }
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
            if !LANDING_PAGE_HTML.starts_with("<html><body><h1>Malkuth</h1>") {
                return serve_spa(&state, &lang, allowed).await;
            }
            return serve_landing(&lang, &state).await;
        }
    }

    if !LANDING_PAGE_HTML.starts_with("<html><body><h1>Malkuth</h1>") {
        return serve_spa(&state, &lang, serve_allowed).await;
    }

    // Normal (non-serve) rendering below
    let i18n = get_i18n(&lang);

    let ready = state.status == InfoStatus::Ready;
    let landing = state.status == InfoStatus::Landing;
    let task = match state.status {
        InfoStatus::Ready => i18n.get("task_idle").map_or("Idle", |v| v.as_str()),
        InfoStatus::Working => i18n
            .get("task_working")
            .map_or("Starting / Restarting", |v| v.as_str()),
        InfoStatus::Landing => i18n.get("task_landing").map_or("Landing", |v| v.as_str()),
    };

    let mut context = tera::Context::new();
    context.insert("lang", &lang);
    context.insert("dir", if lang == "ar" { "rtl" } else { "ltr" });
    context.insert("title", i18n.get("title").map_or("Malkuth", |v| v.as_str()));
    context.insert(
        "heading",
        i18n.get("heading").map_or("Malkuth", |v| v.as_str()),
    );
    context.insert("tagline", i18n.get("tagline").map_or("", |v| v.as_str()));
    context.insert(
        "version_label",
        i18n.get("version").map_or("Version", |v| v.as_str()),
    );
    context.insert(
        "task_label",
        i18n.get("task").map_or("Current Task", |v| v.as_str()),
    );
    let retry_full = i18n.get("retry").map_or("", |v| v.as_str());
    let (retry_before, retry_after) = retry_full.split_once("{n}").unwrap_or((retry_full, ""));
    context.insert("retry_before", retry_before);
    context.insert("retry_after", retry_after);
    context.insert("footer", i18n.get("footer").map_or("", |v| v.as_str()));
    context.insert(
        "status_text",
        if landing {
            i18n.get("status_landing")
                .map_or("Redirecting shortly", |v| v.as_str())
        } else if ready {
            i18n.get("status_ready")
                .map_or("All services running.", |v| v.as_str())
        } else {
            i18n.get("status_starting")
                .map_or("Starting...", |v| v.as_str())
        },
    );
    context.insert("ready", &ready);
    context.insert("landing", &landing);
    context.insert("initial_state", "");
    context.insert("task", task);
    context.insert("version", &state.version);
    // No --serve backend on this path: the cookie token stays at its
    // legacy constant so the template renders a valid cookie write.
    context.insert("malkuth_epoch", "1");

    let install_method = detect_install_method();
    context.insert(
        "install_label",
        i18n.get("install_label")
            .map_or("Installed via", |v| v.as_str()),
    );
    context.insert("install_method", install_method);
    context.insert("binaries", &state.binaries);

    context.insert(
        "binaries_title",
        i18n.get("binaries_title")
            .map_or("Supervised Binaries", |v| v.as_str()),
    );
    let redirect_full = i18n.get("redirect_before").map_or("", |v| v.as_str());
    let (redirect_before, redirect_after) = (
        redirect_full,
        i18n.get("redirect_after")
            .map_or("seconds...", |v| v.as_str()),
    );
    context.insert("redirect_before", redirect_before);
    context.insert("redirect_after", redirect_after);
    context.insert(
        "cancel_label",
        i18n.get("cancel_label").map_or("Cancel", |v| v.as_str()),
    );
    context.insert(
        "refresh_label",
        i18n.get("refresh_label")
            .map_or("Refresh Now", |v| v.as_str()),
    );
    context.insert(
        "retry_unit",
        i18n.get("retry_unit").map_or("seconds", |v| v.as_str()),
    );
    context.insert(
        "retry_manual",
        i18n.get("retry_manual")
            .map_or("You can also refresh manually.", |v| v.as_str()),
    );
    context.insert(
        "copy_hint",
        i18n.get("copy_hint")
            .map_or("Click to copy", |v| v.as_str()),
    );
    context.insert(
        "copied_msg",
        i18n.get("copied_msg")
            .map_or("Copied to clipboard", |v| v.as_str()),
    );
    context.insert(
        "copy_fail_msg",
        i18n.get("copy_fail_msg")
            .map_or("Copy failed", |v| v.as_str()),
    );
    let footer_full = i18n.get("footer").map_or("", |v| v.as_str());
    if let Some((before, after)) = footer_full.split_once("Malkuth") {
        context.insert("footer_prefix", before);
        context.insert("footer_suffix", after);
    } else {
        context.insert("footer_prefix", footer_full);
        context.insert("footer_suffix", "");
    }

    context.insert("logo_base64", &base64_encode(LOGO_BYTES));

    context.insert(
        "proxy_label",
        i18n.get("proxy_label").unwrap_or(&"Proxy".to_string()),
    );
    context.insert(
        "watch_label",
        i18n.get("watch_label").unwrap_or(&"Watching".to_string()),
    );
    if let Some(ref ep) = state.proxy_endpoint {
        context.insert("proxy_endpoint", ep);
    }
    if !state.watch_paths.is_empty() {
        context.insert("watch_paths", &state.watch_paths);
    }

    match tera::Tera::one_off(TEMPLATE, &context, false) {
        Ok(html) => Html(html).into_response(),
        Err(e) => {
            tracing::error!(error = %e, "Failed to render info page template");
            (
                StatusCode::INTERNAL_SERVER_ERROR,
                format!("Template error: {e}"),
            )
                .into_response()
        }
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
        for lang in SUPPORTED {
            let i18n = get_i18n(lang);
            assert!(
                i18n.contains_key("title"),
                "Missing 'title' for language: {lang}"
            );
            assert!(
                i18n.contains_key("heading"),
                "Missing 'heading' for language: {lang}"
            );
        }
    }

    #[test]
    fn test_template_renders() {
        let mut ctx = tera::Context::new();
        ctx.insert("lang", "en");
        ctx.insert("dir", "ltr");
        ctx.insert("title", "Malkuth");
        ctx.insert("heading", "Malkuth");
        ctx.insert("tagline", "Supervisor");
        ctx.insert("version_label", "Version");
        ctx.insert("task_label", "Task");
        ctx.insert("retry_before", "Auto-refresh in ");
        ctx.insert("retry_after", " seconds...");
        ctx.insert("footer", "GitHub");
        ctx.insert("status_text", "Starting...");
        ctx.insert("ready", &false);
        ctx.insert("landing", &false);
        ctx.insert("initial_state", "");
        ctx.insert("task", "Startup");
        ctx.insert("logo_base64", "dGVzdA==");
        ctx.insert("malkuth_epoch", "1");
        ctx.insert("proxy_label", "Proxy");
        ctx.insert("watch_label", "Watching");
        ctx.insert("version", "0.2.0");
        ctx.insert("binaries", &Vec::<serde_json::Value>::new());
        ctx.insert("binaries_title", "Supervised Binaries");
        ctx.insert("redirect_before", "Redirecting in");
        ctx.insert("redirect_after", "seconds...");
        ctx.insert("cancel_label", "Cancel");
        ctx.insert("refresh_label", "Refresh Now");
        ctx.insert("retry_unit", "seconds");
        ctx.insert("retry_manual", "You can also refresh manually.");
        ctx.insert("copy_hint", "Click to copy");
        ctx.insert("copied_msg", "Copied to clipboard");
        ctx.insert("copy_fail_msg", "Copy failed");
        ctx.insert("footer_prefix", "Powered by ");
        ctx.insert(
            "footer_suffix",
            " — a composable service-supervision toolkit for Rust",
        );

        match tera::Tera::one_off(TEMPLATE, &ctx, false) {
            Ok(html) => println!("OK: {} bytes", html.len()),
            Err(e) => panic!("Template error: {:#?}", e),
        }
    }

    /// When the backend is unreachable the serve-mode landing page must not
    /// render a redirect countdown; it shows the refresh action instead.
    #[test]
    fn test_template_renders_offline_without_countdown() {
        let mut ctx = tera::Context::new();
        ctx.insert("lang", "zh-Hans");
        ctx.insert("dir", "ltr");
        ctx.insert("title", "Malkuth");
        ctx.insert("heading", "Malkuth");
        ctx.insert("tagline", "Supervisor");
        ctx.insert("version_label", "Version");
        ctx.insert("task_label", "");
        ctx.insert("retry_before", "");
        ctx.insert("retry_after", "");
        ctx.insert("retry_unit", "");
        ctx.insert("retry_manual", "");
        ctx.insert("footer", "");
        ctx.insert("status_text", "当前服务暂不可达");
        ctx.insert("ready", &false);
        ctx.insert("landing", &false);
        ctx.insert("initial_state", "offline");
        ctx.insert("task", "Landing");
        ctx.insert("logo_base64", "dGVzdA==");
        ctx.insert("malkuth_epoch", "1");
        ctx.insert("proxy_label", "Proxy");
        ctx.insert("watch_label", "Watching");
        ctx.insert("version", "0.2.8");
        ctx.insert("binaries", &Vec::<serde_json::Value>::new());
        ctx.insert("binaries_title", "Supervised Binaries");
        ctx.insert("redirect_before", "将在");
        ctx.insert("redirect_after", "秒后跳转");
        ctx.insert("cancel_label", "取消跳转");
        ctx.insert("refresh_label", "立即刷新");
        ctx.insert("copy_hint", "Click to copy");
        ctx.insert("copied_msg", "Copied");
        ctx.insert("copy_fail_msg", "Copy failed");
        ctx.insert("footer_prefix", "");
        ctx.insert("footer_suffix", "");

        let html = tera::Tera::one_off(TEMPLATE, &ctx, false).expect("Template error");
        assert!(
            !html.contains("id=\"retryHint\""),
            "offline page must not render the countdown hint"
        );
        assert!(
            html.contains(">立即刷新</button>"),
            "offline page must offer the refresh action"
        );
        assert!(
            !html.contains(">取消跳转</button>"),
            "offline page must not offer a cancel-redirect action"
        );
    }

    /// A backend that reports not-ready (starting / draining) must not render
    /// a redirect countdown either; the page polls until it turns ready.
    #[test]
    fn test_template_renders_starting_without_countdown() {
        let mut ctx = tera::Context::new();
        ctx.insert("lang", "zh-Hans");
        ctx.insert("dir", "ltr");
        ctx.insert("title", "Malkuth");
        ctx.insert("heading", "Malkuth");
        ctx.insert("tagline", "Supervisor");
        ctx.insert("version_label", "Version");
        ctx.insert("task_label", "");
        ctx.insert("retry_before", "");
        ctx.insert("retry_after", "");
        ctx.insert("retry_unit", "");
        ctx.insert("retry_manual", "");
        ctx.insert("footer", "");
        ctx.insert("status_text", "服务正在启动中");
        ctx.insert("ready", &false);
        ctx.insert("landing", &false);
        ctx.insert("initial_state", "starting");
        ctx.insert("task", "Landing");
        ctx.insert("logo_base64", "dGVzdA==");
        ctx.insert("malkuth_epoch", "1");
        ctx.insert("proxy_label", "Proxy");
        ctx.insert("watch_label", "Watching");
        ctx.insert("version", "0.2.10");
        ctx.insert("binaries", &Vec::<serde_json::Value>::new());
        ctx.insert("binaries_title", "Supervised Binaries");
        ctx.insert("redirect_before", "将在");
        ctx.insert("redirect_after", "秒后跳转");
        ctx.insert("cancel_label", "取消跳转");
        ctx.insert("refresh_label", "立即刷新");
        ctx.insert("copy_hint", "Click to copy");
        ctx.insert("copied_msg", "Copied");
        ctx.insert("copy_fail_msg", "Copy failed");
        ctx.insert("footer_prefix", "");
        ctx.insert("footer_suffix", "");

        let html = tera::Tera::one_off(TEMPLATE, &ctx, false).expect("Template error");
        assert!(
            !html.contains("id=\"retryHint\""),
            "starting page must not render the countdown hint"
        );
        assert!(
            html.contains(">立即刷新</button>"),
            "starting page must offer the refresh action"
        );
        assert!(
            !html.contains(">取消跳转</button>"),
            "starting page must not offer a cancel-redirect action"
        );
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
        let client = reqwest::Client::builder()
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
            let client = reqwest::Client::builder()
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
}
