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
pub fn info_router(
    version: impl Into<String>,
    status: InfoStatus,
    watch_paths: Vec<String>,
    proxy_endpoint: Option<String>,
    binaries: Vec<BinaryInfo>,
) -> Router<()> {
    let state = InfoState {
        version: version.into(),
        status,
        watch_paths,
        proxy_endpoint,
        binaries,
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
}

async fn info_page(state: axum::extract::State<InfoState>, req: Request) -> Response {
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
