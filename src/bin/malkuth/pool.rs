//! Pod pool manager: spawns N copies of the wrapped command, each on its own
//! backend port (via an env var), probes readiness by TCP-connecting that port,
//! registers healthy pods with the proxy, and supports graceful rolling restart.
//!
//! Concurrency model (no deadlocks): each pod id has its own supervision task
//! that OWNS its child locally and waits on a `select!` of
//! `{ child exit, restart trigger }`. The shared map only holds lightweight
//! metadata (port) for the proxy, and is locked only briefly — never across an
//! `.await` on the child.

use std::{
    collections::HashMap,
    net::SocketAddr,
    process::Stdio,
    sync::Arc,
    time::{Duration, Instant},
};
use tokio::{
    net::TcpStream,
    process::{Child, Command},
    sync::{Mutex, Notify},
    time::sleep,
};

use tracing::{info, warn};

use crate::proxy::{Backend, ProxyState};

/// Lightweight, lock-friendly per-pod metadata exposed to the proxy.
struct PodMeta {
    port: u16,
}

/// Manages the pod pool and keeps the proxy's backend set in sync with healthy pods.
pub struct PodManager {
    host: String,
    port_env: String,
    command: Vec<String>,
    proxy: Option<Arc<ProxyState>>,
    readiness_timeout: Duration,
    /// id -> assigned backend port.
    ports: HashMap<usize, u16>,
    /// Currently-registered (healthy) pods, for the proxy.
    meta: Mutex<HashMap<usize, PodMeta>>,
    /// Per-pod restart trigger.
    restart: HashMap<usize, Arc<Notify>>,
    drain_secs: u64,
}

impl PodManager {
    #[allow(clippy::too_many_arguments)]
    pub fn new(
        host: String,
        port_env: String,
        command: Vec<String>,
        proxy: Option<Arc<ProxyState>>,
        ports: HashMap<usize, u16>,
        drain_secs: u64,
    ) -> Self {
        let restart = ports
            .keys()
            .map(|id| (*id, Arc::new(Notify::new())))
            .collect();
        Self {
            host,
            port_env,
            command,
            proxy,
            readiness_timeout: Duration::from_secs(30),
            meta: Mutex::new(HashMap::new()),
            ports,
            restart,
            drain_secs,
        }
    }

    /// Spawn one supervision task per pod and return.
    pub async fn run(self: Arc<Self>) {
        for &id in self.ports.keys() {
            let this = Arc::clone(&self);
            tokio::spawn(async move {
                this.supervise(id).await;
            });
        }
    }

    /// Request a rolling restart of one pod (round-robin over ids by the caller).
    pub async fn restart_one(&self, id: usize) {
        info!(pod = id, "rolling restart requested");
        if let Some(n) = self.restart.get(&id) {
            n.notify_one();
        }
    }

    async fn supervise(self: Arc<Self>, id: usize) {
        let Some(&port) = self.ports.get(&id) else {
            return;
        };
        let notify = Arc::clone(&self.restart[&id]);
        loop {
            // spawn + wait for readiness, then register with the proxy.
            let mut child = match self.spawn_pod(id, port).await {
                Ok(c) => c,
                Err(e) => {
                    warn!(pod = id, error = %e, "failed to spawn pod; retrying shortly");
                    sleep(Duration::from_secs(1)).await;
                    continue;
                }
            };
            if self.wait_ready(port).await {
                info!(pod = id, port, "pod ready");
                {
                    let mut meta = self.meta.lock().await;
                    meta.insert(id, PodMeta { port });
                }
                self.publish_backends().await;
            } else {
                warn!(pod = id, port, "pod did not become ready in time");
            }

            // Wait for either a natural exit or an explicit restart request.
            tokio::select! {
                status = child.wait() => {
                    warn!(pod = id, ?status, "pod exited");
                }
                _ = notify.notified() => {
                    info!(pod = id, "draining pod for restart");
                    let _ = child.start_kill();
                    let _ = tokio::time::timeout(Duration::from_secs(self.drain_secs.max(1)), child.wait()).await;
                }
            }

            // Pod gone: deregister + refresh the proxy, brief backoff, respawn.
            {
                let mut meta = self.meta.lock().await;
                meta.remove(&id);
            }
            self.publish_backends().await;
            sleep(Duration::from_millis(150)).await;
        }
    }

    async fn publish_backends(&self) {
        if let Some(proxy) = &self.proxy {
            let meta = self.meta.lock().await;
            proxy.set_backends(backends_from(&self.host, &meta));
        }
    }

    async fn spawn_pod(&self, id: usize, port: u16) -> std::io::Result<Child> {
        let (program, args) = self.command.split_first().ok_or_else(|| {
            std::io::Error::new(std::io::ErrorKind::InvalidInput, "no command given")
        })?;
        let mut cmd = Command::new(program);
        cmd.args(args);
        cmd.env(&self.port_env, port.to_string());
        cmd.env("MALKUTH_POD_ID", format!("pod-{id}"));
        cmd.stdin(Stdio::null());
        cmd.stdout(Stdio::inherit());
        cmd.stderr(Stdio::inherit());
        cmd.kill_on_drop(true);
        info!(pod = id, port, program, "spawning pod");
        cmd.spawn()
    }

    /// Try to TCP-connect to `port` until success or the readiness timeout.
    async fn wait_ready(&self, port: u16) -> bool {
        let addr: SocketAddr = format!("{}:{}", self.host, port)
            .parse()
            .unwrap_or_else(|_| format!("127.0.0.1:{port}").parse().unwrap());
        let deadline = Instant::now() + self.readiness_timeout;
        while Instant::now() < deadline {
            if TcpStream::connect(addr).await.is_ok() {
                return true;
            }
            sleep(Duration::from_millis(150)).await;
        }
        false
    }
}

fn backends_from(host: &str, meta: &HashMap<usize, PodMeta>) -> Vec<Backend> {
    let host_ip = host
        .parse()
        .unwrap_or_else(|_| "127.0.0.1".parse().unwrap());
    meta.iter()
        .map(|(id, m)| Backend {
            addr: SocketAddr::new(host_ip, m.port),
            id: format!("pod-{id}"),
        })
        .collect()
}

/// Assign `count` distinct ports from `ports`, skipping `skip`.
pub fn assign_ports(
    ports: impl Iterator<Item = u16>,
    count: usize,
    skip: u16,
) -> HashMap<usize, u16> {
    ports
        .filter(|p| *p != skip)
        .take(count)
        .enumerate()
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn assign_ports_skips_public() {
        let m = assign_ports(3000..=3005, 3, 3000);
        assert_eq!(m.len(), 3);
        assert_eq!(m[&0], 3001);
        assert_eq!(m[&2], 3003);
    }
}
