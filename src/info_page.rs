use axum::{
    Router,
    extract::Request,
    http::{StatusCode, header},
    response::{Html, IntoResponse, Response},
    routing::get,
};
use std::collections::HashMap;
use std::sync::LazyLock;
use std::time::Duration;

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
///   backend response passes through untouched (status, headers, body)
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

    if let Some(ref backend_url) = serve_backend {
        let state_arc = backend_state.clone();
        let url = backend_url.clone();
        // Probe once synchronously so the very first landing-page request
        // already knows whether the backend is reachable (no Unknown window).
        let initial = probe_backend(&url);
        if let Ok(mut w) = state_arc.try_write() {
            *w = initial;
        }
        tokio::spawn(async move {
            loop {
                // Blocking probe on the blocking thread pool so slow backends
                // never stall an async worker.
                let probe_url = url.clone();
                let state = tokio::task::spawn_blocking(move || probe_backend(&probe_url))
                    .await
                    .unwrap_or(BackendState::Down);
                let mut w = state_arc.write().await;
                *w = state;
                drop(w);
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
    };
    Router::new()
        .route("/", get(info_page))
        .fallback(get(info_page))
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

async fn build_spa_init(state: &InfoState, lang: &str) -> serde_json::Value {
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
        "logo_base64": base64_encode(LOGO_BYTES),
    })
}

async fn serve_spa(state: &InfoState, lang: &str) -> Response {
    let init_data = build_spa_init(state, lang).await;
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

async fn proxy_to_backend(req: Request, backend: &str) -> Result<Response, ()> {
    let uri = req.uri();
    let path = uri.path_and_query().map_or("/", |pq| pq.as_str());
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

    let (parts, body) = req.into_parts();
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

    response.headers_mut().append(
        header::SET_COOKIE,
        header::HeaderValue::from_static("__malkuth_nonce=1; max-age=1800; path=/"),
    );
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

/// Read the `__malkuth_nonce` cookie value as a retry counter (0 = first visit).
fn read_nonce(req: &Request) -> u8 {
    let cookie_header = req.headers().get(header::COOKIE);
    let Some(cookies) = cookie_header.and_then(|v| v.to_str().ok()) else {
        return 0;
    };
    for part in cookies.split(';') {
        let kv = part.trim();
        if let Some(val) = kv.strip_prefix("__malkuth_nonce=") {
            return val.parse::<u8>().unwrap_or(0);
        }
    }
    0
}

/// JSON probe endpoint for polling landing page.
async fn serve_probe(lang: &str, state: &InfoState) -> Response {
    let backend_state = *state.backend_state.read().await;
    let backend_up = state.serve_backend.is_some() && backend_state == BackendState::Up;

    let (probe_state, msg) = if !state.serve_backend.is_some() || backend_up {
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

    let init_state: &str = if !state.serve_backend.is_some() || backend_up {
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
    ctx.insert("landing_nonce", &0u8);

    match tera::Tera::one_off(TEMPLATE, &ctx, false) {
        Ok(html) => Html(html).into_response(),
        Err(e) => (StatusCode::INTERNAL_SERVER_ERROR, format!("Failed: {e}")).into_response(),
    }
}

async fn info_page(state: axum::extract::State<InfoState>, req: Request) -> Response {
    let lang = detect_language(req.headers());

    if req.headers().get("x-malkuth-probe").is_some() {
        return serve_probe(&lang, &state).await;
    }

    if let Some(ref backend) = state.serve_backend {
        let allowed = state.serve_hosts.is_empty()
            || state.serve_hosts.iter().any(|h| {
                req.headers()
                    .get(header::HOST)
                    .is_some_and(|v| v.as_bytes() == h.as_bytes())
            });
        if allowed {
            let nonce = read_nonce(&req);
            if nonce == 1 {
                if let Ok(resp) = proxy_to_backend(req, backend).await {
                    return resp;
                }
            }
            if !LANDING_PAGE_HTML.starts_with("<html><body><h1>Malkuth</h1>") {
                return serve_spa(&state, &lang).await;
            }
            return serve_landing(&lang, &state).await;
        }
    }

    if !LANDING_PAGE_HTML.starts_with("<html><body><h1>Malkuth</h1>") {
        return serve_spa(&state, &lang).await;
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
    context.insert("landing_nonce", &0u8);
    context.insert("initial_state", "");
    context.insert("task", task);
    context.insert("version", &state.version);

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
        let resp = proxy_to_backend(req, &format!("http://{backend}"))
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
            let resp = proxy_to_backend(req, &format!("http://{backend}"))
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
        let resp = proxy_to_backend(req, &format!("http://{backend}"))
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
                proxy_to_backend(req, &format!("http://{backend}"))
                    .await
                    .is_err(),
                "status `{status_line}` must fall back to the landing page"
            );
        }
    }
}
