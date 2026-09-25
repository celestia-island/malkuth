//! L4 TCP reverse proxy with sticky (client-IP) routing via consistent hashing.
//!
//! Listens on a public port and forwards each connection to one of a set of
//! healthy backends. A backend is chosen by hashing the client's IP onto a
//! consistent-hash ring of virtual nodes, so:
//!   - the same client IP keeps landing on the same backend (sticky), and
//!   - adding/removing a backend only moves the keys that backend owned
//!     (minimal disruption — "won't switch unless the node restarts/scales down").

use std::{
    collections::HashMap,
    net::SocketAddr,
    sync::{Arc, RwLock},
    time::{Duration, Instant},
};
use tokio::{
    io,
    net::{TcpListener, TcpStream},
};

use tracing::{debug, info, warn};

/// Virtual nodes per backend on the ring.
const VNODES: usize = 160;

/// A backend endpoint the proxy can forward to.
#[derive(Debug, Clone, PartialEq, Eq, Hash)]
pub struct Backend {
    pub addr: SocketAddr,
    pub id: String,
}

/// Consistent-hash ring over the current set of backends.
#[derive(Default)]
pub struct Ring {
    points: Vec<(u64, usize)>,
    backends: Vec<Backend>,
}

impl Ring {
    pub fn from_backends(backends: Vec<Backend>) -> Self {
        let mut points = Vec::with_capacity(backends.len() * VNODES);
        for (i, b) in backends.iter().enumerate() {
            for vn in 0..VNODES {
                points.push((hash64(format!("{}/{}", b.id, vn)), i));
            }
        }
        points.sort_unstable_by_key(|(h, _)| *h);
        Self { points, backends }
    }

    pub fn backends(&self) -> &[Backend] {
        &self.backends
    }

    /// Pick the backend owning `key` (first point ≥ hash(key), wrapping).
    #[allow(dead_code)]
    pub fn route(&self, key: &str) -> Option<&Backend> {
        if self.points.is_empty() {
            return None;
        }
        let h = hash64(key);
        let idx = self.points.partition_point(|(p, _)| *p < h);
        let (_, i) = self.points[idx % self.points.len()];
        self.backends.get(i)
    }

    /// Pick a backend for `key`, skipping any in `exclude`.
    pub fn route_excluding(&self, key: &str, exclude: &[SocketAddr]) -> Option<&Backend> {
        if self.points.is_empty() {
            return None;
        }
        let h = hash64(key);
        let start = self.points.partition_point(|(p, _)| *p < h);
        let n = self.points.len();
        for off in 0..n {
            let (_, i) = self.points[(start + off) % n];
            if let Some(b) = self.backends.get(i) {
                if !exclude.contains(&b.addr) {
                    return Some(b);
                }
            }
        }
        None
    }
}

/// Shared proxy state: the ring + a sticky client→backend cache.
pub struct ProxyState {
    ring: RwLock<Arc<Ring>>,
    sticky: RwLock<HashMap<String, (SocketAddr, Instant)>>,
    ttl: Duration,
}

impl ProxyState {
    pub fn new(ttl: Duration) -> Self {
        Self {
            ring: RwLock::new(Arc::new(Ring::default())),
            sticky: RwLock::new(HashMap::new()),
            ttl,
        }
    }

    /// Swap in a fresh ring built from `backends`. Sticky mappings for surviving
    /// backends are kept; only dead ones get re-routed lazily.
    pub fn set_backends(&self, backends: Vec<Backend>) {
        let new = Arc::new(Ring::from_backends(backends));
        if let Ok(mut g) = self.ring.write() {
            *g = new;
        }
    }

    fn snapshot(&self) -> Arc<Ring> {
        self.ring
            .read()
            .map(|g| Arc::clone(&g))
            .unwrap_or_else(|_| Arc::new(Ring::default()))
    }

    /// Choose a backend for `client_ip`, preferring the sticky cache and
    /// skipping any in `dead`. Records a fresh sticky entry on a new pick.
    pub fn pick(&self, client_ip: &str, dead: &[SocketAddr]) -> Option<Backend> {
        // 1. sticky cache hit?
        let sticky_hit = self
            .sticky
            .read()
            .ok()
            .and_then(|g| g.get(client_ip).copied())
            .filter(|(addr, exp)| *exp > Instant::now() && !dead.contains(addr));
        if let Some((addr, _)) = sticky_hit {
            let ring = self.snapshot();
            if ring.backends().iter().any(|b| b.addr == addr) {
                return Some(Backend {
                    addr,
                    id: String::new(),
                });
            }
        }
        // 2. consistent-hash route.
        let ring = self.snapshot();
        let chosen = ring.route_excluding(client_ip, dead)?;
        if let Ok(mut g) = self.sticky.write() {
            g.insert(
                client_ip.to_string(),
                (chosen.addr, Instant::now() + self.ttl),
            );
        }
        Some(chosen.clone())
    }

    /// Forget the sticky mapping for `client_ip`.
    pub fn invalidate(&self, client_ip: &str) {
        if let Ok(mut g) = self.sticky.write() {
            g.remove(client_ip);
        }
    }
}

/// Run the proxy on `public` until the process exits.
pub async fn run_proxy(public: SocketAddr, state: Arc<ProxyState>) -> io::Result<()> {
    let listener = TcpListener::bind(public).await?;
    run_proxy_on(listener, state).await
}

/// Listener-injected variant (tests bind an ephemeral port).
pub async fn run_proxy_on(listener: TcpListener, state: Arc<ProxyState>) -> io::Result<()> {
    let public = listener.local_addr()?;
    info!(event = "proxy_listening", %public, "sticky reverse proxy accepting");
    loop {
        let (client, peer) = match listener.accept().await {
            Ok(v) => v,
            Err(e) => {
                warn!(error = %e, "proxy accept failed");
                continue;
            }
        };
        let state = Arc::clone(&state);
        tokio::spawn(async move {
            if let Err(e) = handle_client(client, peer, state).await {
                debug!(error = %e, "proxy connection ended");
            }
        });
    }
}

async fn handle_client(
    mut client: TcpStream,
    peer: SocketAddr,
    state: Arc<ProxyState>,
) -> io::Result<()> {
    let client_ip = peer.ip().to_string();
    let mut dead: Vec<SocketAddr> = Vec::new();
    // Try backends until one connects; mark failures dead + invalidate sticky.
    let mut upstream = loop {
        let backend = match state.pick(&client_ip, &dead) {
            Some(b) => b,
            None => {
                // No backend answered (pod restarting / crashed). Closing the
                // connection degrades into a bare 502 at whatever fronts this
                // proxy — serve an explicit maintenance page instead, so the
                // operator's browser pauses briefly and self-retries.
                debug!(%peer, "no healthy backend; serving maintenance page");
                let _ = client.write_all(maintenance_response().as_slice()).await;
                let _ = client.shutdown().await;
                return Ok(());
            }
        };
        match TcpStream::connect(backend.addr).await {
            Ok(s) => break s,
            Err(e) => {
                warn!(backend = %backend.addr, error = %e, "backend connect failed; trying another");
                dead.push(backend.addr);
                state.invalidate(&client_ip);
            }
        }
    };
    let _ = io::copy_bidirectional(&mut client, &mut upstream).await?;
    Ok(())
}

use tokio::io::AsyncWriteExt as _;

/// The no-backend maintenance response: HTTP 503 with `Retry-After` and a
/// self-contained page that reloads the original URL automatically, so a pod
/// restart (seconds) degrades into a brief pause instead of a blanket 502.
/// Byte-oriented by design — the proxy is protocol-agnostic and this is the
/// one place it speaks HTTP.
fn maintenance_response() -> Vec<u8> {
    let body = MAINTENANCE_BODY;
    format!(
        "HTTP/1.1 503 Service Unavailable\r\n\
         Content-Type: text/html; charset=utf-8\r\n\
         Cache-Control: no-store\r\n\
         Retry-After: 3\r\n\
         X-Content-Type-Options: nosniff\r\n\
         Connection: close\r\n\
         Content-Length: {}\r\n\
         \r\n\
         {body}",
        body.len()
    )
    .into_bytes()
}

const MAINTENANCE_BODY: &str = r#"<!doctype html>
<html lang="en">
<head>
<meta charset="utf-8">
<meta name="viewport" content="width=device-width, initial-scale=1">
<meta name="color-scheme" content="dark light">
<meta http-equiv="refresh" content="3">
<title>Malkuth — service restarting</title>
<style>
  *{box-sizing:border-box}html,body{height:100%}
  :root{--bg:#101418;--fg:#e8eaf0;--muted:rgba(203,213,225,.8);--faint:rgba(148,163,184,.7);
    --ring:rgba(148,163,184,.25);--ring-top:#7aa2f7}
  /* Light palette mirrors the info page's light scheme (bg/fg tokens). */
  @media (prefers-color-scheme: light){
    :root{--bg:#f5f5f0;--fg:#333340;--muted:rgba(30,30,50,.75);--faint:rgba(30,30,50,.55);
      --ring:rgba(30,30,50,.14);--ring-top:#4c6ef5}
  }
  body{margin:0;display:grid;place-items:center;padding:24px;background:var(--bg);color:var(--fg);
    font:15px/1.7 system-ui,-apple-system,"Segoe UI","PingFang SC","Microsoft YaHei",sans-serif}
  main{text-align:center;max-width:34rem}
  .ring{width:44px;height:44px;margin:0 auto 18px;border-radius:50%;
    border:3px solid var(--ring);border-top-color:var(--ring-top);animation:spin 1s linear infinite}
  h1{font-size:18px;font-weight:600;margin:0}
  p{color:var(--muted);font-size:14px}
  .alt{color:var(--faint);font-size:12.5px}
  @keyframes spin{to{transform:rotate(360deg)}}
  @media (prefers-reduced-motion:reduce){.ring{animation:none;opacity:.6}}
</style>
</head>
<body>
<main role="status">
  <div class="ring" aria-hidden="true"></div>
  <h1>Service is restarting</h1>
  <p>This page will retry automatically — usually ready within seconds.</p>
  <p class="alt" lang="zh-Hans">服务正在重启，本页会自动重试，通常几秒内即可恢复。</p>
</main>
<script>setTimeout(function(){location.reload()},3000)</script>
</body>
</html>
"#;

/// fnv-1a 64-bit.
fn hash64(s: impl AsRef<str>) -> u64 {
    let mut h: u64 = 0xcbf2_9ce4_8422_2325;
    for &b in s.as_ref().as_bytes() {
        h ^= b as u64;
        h = h.wrapping_mul(0x100_0000_01b3);
    }
    h
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::net::{Ipv4Addr, SocketAddrV4};

    fn be(id: &str, port: u16) -> Backend {
        Backend {
            addr: SocketAddr::V4(SocketAddrV4::new(Ipv4Addr::LOCALHOST, port)),
            id: id.into(),
        }
    }

    #[test]
    fn ring_routes_stably() {
        let ring = Ring::from_backends(vec![be("a", 1), be("b", 2), be("c", 3)]);
        let r1 = ring.route("1.2.3.4").unwrap().id.clone();
        let r2 = ring.route("1.2.3.4").unwrap().id.clone();
        assert_eq!(r1, r2);
    }

    #[test]
    fn adding_backend_moves_few_keys() {
        let small = Ring::from_backends(vec![be("a", 1), be("b", 2)]);
        let big = Ring::from_backends(vec![be("a", 1), be("b", 2), be("c", 3)]);
        let keys: Vec<String> = (0..500).map(|i| format!("10.0.0.{i}")).collect();
        let mut moved = 0;
        for k in &keys {
            if small.route(k).map(|b| b.id.clone()) != big.route(k).map(|b| b.id.clone()) {
                moved += 1;
            }
        }
        assert!(moved < 280, "too many keys moved: {moved}");
    }

    #[test]
    fn route_excluding_skips_dead() {
        let ring = Ring::from_backends(vec![be("a", 1), be("b", 2)]);
        let dead = vec![SocketAddr::V4(SocketAddrV4::new(Ipv4Addr::LOCALHOST, 1))];
        for k in &["x", "y", "z"] {
            let b = ring.route_excluding(k, &dead).unwrap();
            assert_ne!(b.addr.port(), 1);
        }
    }

    #[test]
    fn maintenance_head_declares_the_exact_body_length() {
        let resp = maintenance_response();
        let text = std::str::from_utf8(&resp).unwrap();
        let (head, body) = text.split_once("\r\n\r\n").unwrap();
        let len: usize = head
            .lines()
            .find_map(|l| l.strip_prefix("Content-Length: "))
            .unwrap()
            .parse()
            .unwrap();
        assert_eq!(len, body.len(), "declared vs actual body bytes");
        assert!(head.contains("HTTP/1.1 503 Service Unavailable"));
        assert!(head.contains("Retry-After: 3"));
        assert!(head.contains("X-Content-Type-Options: nosniff"));
        assert!(head.contains("Cache-Control: no-store"));
    }

    #[test]
    fn maintenance_page_honors_the_light_color_scheme() {
        // Regression guard for #138: the maintenance page shipped dark-only
        // (`color-scheme: dark` + hardcoded dark palette), so light-mode
        // visitors saw an all-dark page for the whole restart window even
        // though the info page supports both schemes.
        let body = MAINTENANCE_BODY;
        assert!(
            body.contains(r#"<meta name="color-scheme" content="dark light">"#),
            "color-scheme meta must declare both schemes"
        );
        let (before_light, from_light) = body
            .split_once("@media (prefers-color-scheme: light)")
            .expect("light-scheme media query must exist");
        // Exactly one light query, and no unresolved conflict markers: a
        // both-sides-kept merge that duplicates the block (drifted copy LAST
        // wins the cascade) or leaves `<<<<<<<` in place would otherwise pass
        // every pin below while rendering the wrong palette.
        assert_eq!(
            body.matches("@media (prefers-color-scheme: light)").count(),
            1,
            "exactly one light media query expected (duplicate block?)"
        );
        assert!(
            !body.contains("<<<<<<<") && !body.contains(">>>>>>>"),
            "unresolved merge-conflict markers in the page"
        );
        // The match must sit in LIVE CSS, not inside an unclosed comment: a
        // merge-conflict resolution that comments out the WHOLE @media
        // construct still lets split_once match the commented text, so the
        // guard would otherwise pin a dead block. Unclosed comment spanning
        // the match ⇔ the last "/*" in before_light is not followed by "*/".
        let unclosed = match (before_light.rfind("/*"), before_light.rfind("*/")) {
            (Some(open), Some(close)) => open > close,
            (Some(_), None) => true,
            _ => false,
        };
        assert!(
            !unclosed,
            "light media query sits inside an unclosed CSS comment"
        );
        // Comment balance across the WHOLE style element: a dropped `*/`
        // anywhere after the light block silently kills every consumption
        // rule (body/.ring/p/.alt) while textual contains-pins still match
        // the commented text. Balanced pairs that comment out a single rule
        // are NOT caught — that failure is visually total (unstyled page in
        // both schemes), unlike the silent wrong-palette regression this
        // guard targets.
        let style = body
            .split_once("<style>")
            .and_then(|(_, rest)| rest.split_once("</style>").map(|(s, _)| s))
            .expect("<style> element");
        assert_eq!(
            style.matches("/*").count(),
            style.matches("*/").count(),
            "unbalanced CSS comments in <style> — a rule set is dead"
        );
        // The light block is the `:root{...}` rule right after the query; the
        // closing brace of that rule ends the slice. Pin the whole declaration
        // set (whitespace-insensitive) instead of checking token presence: an
        // interior comment-out, a reorder, or a missing token must all fail.
        let light_block = from_light.split('}').next().unwrap_or_default();
        assert!(
            !light_block.contains("/*") && !light_block.contains("*/"),
            "light block must not contain comment markers"
        );
        let light_decls: String = light_block.chars().filter(|c| !c.is_whitespace()).collect();
        // bg/fg derive from the info page's light scheme so "mirrors the info
        // page" is a machine-checked relation rather than a restated literal;
        // the two asserts below self-prove the parse (a broken pattern cannot
        // read as green). Accent tokens stay literal — a deliberate commit
        // that moves both the page and this expectation is out of scope for
        // a regression guard.
        let info_decls: String = include_str!("../../info_page/template.html")
            .split_once("@media (prefers-color-scheme: light)")
            .expect("info page must keep a light scheme")
            .1
            .split('}')
            .next()
            .unwrap_or_default()
            .chars()
            .filter(|c| !c.is_whitespace())
            .collect();
        fn decl_token<'a>(decls: &'a str, name: &str) -> &'a str {
            decls
                .split_once(&format!("{name}:"))
                .map_or("", |(_, rest)| rest.split(';').next().unwrap_or(""))
        }
        assert_eq!(
            decl_token(&info_decls, "--bg"),
            "#f5f5f0",
            "info-page parse self-proof (bg)"
        );
        assert_eq!(
            decl_token(&info_decls, "--fg"),
            "#333340",
            "info-page parse self-proof (fg)"
        );
        assert_eq!(
            light_decls,
            format!(
                "{{:root{{--bg:{};--fg:{};--muted:rgba(30,30,50,.75);\
                 --faint:rgba(30,30,50,.55);--ring:rgba(30,30,50,.14);\
                 --ring-top:#4c6ef5",
                decl_token(&info_decls, "--bg"),
                decl_token(&info_decls, "--fg"),
            ),
            "light block must be exactly this declaration set (no comments, no value drift)"
        );
        // Dark stays the default outside the light query.
        assert!(
            before_light.contains("--bg:#101418"),
            "dark tokens remain the default outside the light block"
        );
        // Definitions alone do not render anything — pin the consumption side
        // too, so a partial revert to hardcoded dark values (while keeping the
        // variable definitions) cannot silently re-break light mode.
        for consumed in [
            "background:var(--bg);color:var(--fg)",
            "border:3px solid var(--ring);border-top-color:var(--ring-top)",
            "p{color:var(--muted)",
            ".alt{color:var(--faint)",
        ] {
            assert!(
                body.contains(consumed),
                "page must consume the theme variables: missing `{consumed}`"
            );
        }
    }

    #[tokio::test]
    async fn no_backend_serves_the_maintenance_page_instead_of_hanging_up() {
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = listener.local_addr().unwrap();
        let state = std::sync::Arc::new(ProxyState::new(Duration::from_secs(30)));
        // Empty ring: every client must get the maintenance page.
        let server = tokio::spawn(run_proxy_on(listener, state));

        let mut c = tokio::net::TcpStream::connect(addr).await.unwrap();
        use tokio::io::{AsyncReadExt, AsyncWriteExt};
        c.write_all(b"GET / HTTP/1.1\r\nHost: demo\r\n\r\n")
            .await
            .unwrap();
        let mut seen = Vec::new();
        c.read_to_end(&mut seen).await.unwrap();
        let text = String::from_utf8_lossy(&seen);
        assert!(text.starts_with("HTTP/1.1 503"), "got: {text}");
        assert!(text.contains("Service is restarting"), "{text}");
        assert!(text.contains("服务正在重启"), "{text}");
        server.abort();
    }
}
