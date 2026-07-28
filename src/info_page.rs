use axum::{
    Router,
    extract::Request,
    http::{StatusCode, header},
    response::{Html, IntoResponse, Response},
    routing::get,
};
use std::collections::HashMap;
use std::sync::LazyLock;

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
/// - Backend reachable (TCP connect succeeds) → forward the request
/// - Backend unreachable → render the info/landing page
pub fn info_router(
    version: impl Into<String>,
    status: InfoStatus,
    watch_paths: Vec<String>,
    proxy_endpoint: Option<String>,
    binaries: Vec<BinaryInfo>,
    serve_backend: Option<String>,
    serve_hosts: Vec<String>,
) -> Router<()> {
    let state = InfoState {
        version: version.into(),
        status,
        watch_paths,
        proxy_endpoint,
        binaries,
        serve_backend,
        serve_hosts,
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
}

async fn proxy_to_backend(req: Request, backend: &str) -> Result<Response, ()> {
    let uri = req.uri();
    let path = uri.path_and_query().map_or("/", |pq| pq.as_str());
    let url = format!("{}{}", backend.trim_end_matches('/'), path);

    let client = reqwest::Client::builder()
        .no_proxy()
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

    // Treat 4xx/5xx as backend-unhealthy → fall back to landing
    if status_code >= 400 {
        return Err(());
    }

    let status = StatusCode::from_u16(status_code).unwrap_or(StatusCode::OK);
    let resp_headers: Vec<(String, Vec<u8>)> = resp
        .headers()
        .iter()
        .filter(|(n, _)| {
            let lower = n.as_str().to_lowercase();
            lower != "transfer-encoding" && lower != "connection"
        })
        .map(|(n, v)| (n.as_str().to_string(), v.as_bytes().to_vec()))
        .collect();
    let resp_body = resp.bytes().await.map_err(|_| ())?;

    let mut response = Response::new(axum::body::Body::from(resp_body));
    *response.status_mut() = status;

    response.headers_mut().insert(
        header::SET_COOKIE,
        header::HeaderValue::from_static("__malkuth_nonce=1; max-age=1800; path=/"),
    );

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
fn serve_probe(lang: &str, state: &InfoState, req: &Request) -> Response {
    let nonce = read_nonce(req);

    let backend_up = state.serve_backend.as_ref().map_or(false, |url| {
        let host_port = url
            .trim_start_matches("http://")
            .trim_start_matches("https://");
        let addr: std::net::SocketAddr = match host_port.parse().ok() {
            Some(a) => a,
            None => return false,
        };
        let mut stream = match std::net::TcpStream::connect_timeout(
            &addr,
            std::time::Duration::from_millis(500),
        ) {
            Ok(s) => s,
            Err(_) => return false,
        };
        use std::io::{Read, Write};
        let _ = stream.set_read_timeout(Some(std::time::Duration::from_millis(500)));
        let req = format!("GET / HTTP/1.0\r\nHost: {}\r\n\r\n", host_port);
        if stream.write_all(req.as_bytes()).is_err() {
            return false;
        }
        let mut buf = [0u8; 64];
        let n = stream.read(&mut buf).unwrap_or(0);
        let head = std::str::from_utf8(&buf[..n]).unwrap_or("");
        head.contains(" 200 ") || head.contains(" 3")
    });

    let (probe_state, msg) = if nonce >= 3 {
        (
            "offline",
            get_i18n(lang)
                .get("status_starting")
                .map_or("Service temporarily unavailable", |v| v.as_str()),
        )
    } else if nonce > 0 && backend_up {
        ("ready", "")
    } else if nonce > 0 {
        ("building", "")
    } else {
        ("landing", "")
    };

    let message = if msg.is_empty() {
        let i18n = get_i18n(lang);
        match probe_state {
            "landing" => i18n
                .get("status_landing")
                .map_or("Redirecting shortly", |v| v.as_str())
                .to_string(),
            "building" => i18n
                .get("status_building")
                .map_or("Building...", |v| v.as_str())
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

    let json = serde_json::json!({
        "state": probe_state,
        "nonce": nonce + 1,
        "message": message,
        "progress": null,
    })
    .to_string();

    let mut resp = Response::new(axum::body::Body::from(json));
    resp.headers_mut().insert(
        header::CONTENT_TYPE,
        header::HeaderValue::from_static("application/json"),
    );
    resp
}

/// Serve the landing page. `nonce` is the current retry count (0 → first visit).
/// The template receives `landing_nonce` to:
/// - Set the cookie via JS (`document.cookie = "__malkuth_nonce=N"`)
/// - Show "offline" after 3 failed attempts
fn serve_landing(lang: &str, state: &InfoState, nonce: u8) -> Response {
    let i18n = get_i18n(lang);
    let next = nonce + 1;
    let offline = nonce >= 3;

    let mut ctx = tera::Context::new();
    ctx.insert("lang", lang);
    ctx.insert("dir", if lang == "ar" { "rtl" } else { "ltr" });
    ctx.insert("title", i18n.get("title").map_or("Malkuth", |v| v.as_str()));
    ctx.insert(
        "heading",
        i18n.get("heading").map_or("Malkuth", |v| v.as_str()),
    );
    ctx.insert("tagline", i18n.get("tagline").map_or("", |v| v.as_str()));
    ctx.insert("ready", &false);
    ctx.insert("landing", &!offline);
    ctx.insert(
        "task",
        i18n.get("task_landing").map_or("Landing", |v| v.as_str()),
    );
    ctx.insert("version", &state.version);
    ctx.insert(
        "status_text",
        if offline {
            i18n.get("status_building")
                .map_or("Service temporarily unavailable", |v| v.as_str())
        } else {
            i18n.get("status_landing")
                .map_or("Redirecting shortly", |v| v.as_str())
        },
    );
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
    ctx.insert(
        "retry_manual",
        if offline {
            i18n.get("retry_manual")
                .map_or("You can also refresh manually.", |v| v.as_str())
        } else {
            ""
        },
    );
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
    ctx.insert("landing_nonce", &next);

    match tera::Tera::one_off(TEMPLATE, &ctx, false) {
        Ok(html) => Html(html).into_response(),
        Err(e) => (StatusCode::INTERNAL_SERVER_ERROR, format!("Failed: {e}")).into_response(),
    }
}

async fn info_page(state: axum::extract::State<InfoState>, req: Request) -> Response {
    let lang = detect_language(req.headers());

    if req.headers().get("x-malkuth-probe").is_some() {
        return serve_probe(&lang, &state, &req);
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
            if nonce > 0 && nonce < 3 {
                if let Ok(resp) = proxy_to_backend(req, backend).await {
                    return resp;
                }
            }
            return serve_landing(&lang, &state, nonce);
        }
    }

    // Normal (non-serve) rendering below
    let lang = detect_language(req.headers());
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
    "en", "zhs", "zht", "ja", "ko", "fr", "de", "es", "pt", "ru", "ar",
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
        "zh" | "zh-cn" | "zh-hans" | "zh-sg" => "zhs".into(),
        "zh-tw" | "zh-hk" | "zh-mo" | "zh-hant" => "zht".into(),
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

    #[test]
    fn test_detect_zh() {
        let mut headers = header::HeaderMap::new();
        headers.insert("accept-language", "zh-CN,zh;q=0.9".parse().unwrap());
        assert_eq!(detect_language(&headers), "zhs");
    }

    #[test]
    fn test_detect_zht() {
        let mut headers = header::HeaderMap::new();
        headers.insert("accept-language", "zh-TW,zh;q=0.9".parse().unwrap());
        assert_eq!(detect_language(&headers), "zht");
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
}
