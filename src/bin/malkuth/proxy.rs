//! L4 TCP reverse proxy with sticky (client-IP) routing via consistent hashing.
//!
//! Listens on a public port and forwards each connection to one of a set of
//! healthy backends. A backend is chosen by hashing the client's IP onto a
//! consistent-hash ring of virtual nodes, so:
//!   - the same client IP keeps landing on the same backend (sticky), and
//!   - adding/removing a backend only moves the keys that backend owned
//!     (minimal disruption — "won't switch unless the node restarts/scales down").

use std::collections::HashMap;
use std::net::SocketAddr;
use std::sync::{Arc, RwLock};
use std::time::{Duration, Instant};

use tokio::io;
use tokio::net::{TcpListener, TcpStream};
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
                debug!(%peer, "no healthy backend; closing client");
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
}
