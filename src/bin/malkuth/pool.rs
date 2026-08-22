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
    sync::{Arc, Mutex as StdMutex},
    time::{Duration, Instant},
};
use tokio::{
    io::{AsyncBufReadExt, BufReader},
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
    runtime_log: Arc<StdMutex<Vec<String>>>,
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
        runtime_log: Arc<StdMutex<Vec<String>>>,
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
            runtime_log,
        }
    }

    /// Override the initial readiness window (tests only; production keeps
    /// the 30s default).
    #[cfg(test)]
    pub(crate) fn with_readiness_timeout(mut self, d: Duration) -> Self {
        self.readiness_timeout = d;
        self
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
            // Whether the child is still running when readiness handling
            // finishes, and whether a restart was already requested during
            // that window (the request must not be lost).
            let mut pod_alive = true;
            let mut restart_now = false;
            if port == 0 {
                // No --proxy was configured (--serve-only front door), so
                // no backend port was assigned: readiness probing is
                // meaningless — connect() to port 0 can never succeed, and
                // probing forever would strand the supervision task before
                // the select! below (which is the only consumer of restart
                // notifications). Skip straight to supervision.
                info!(
                    pod = id,
                    "no proxy port assigned (--serve-only); readiness probing skipped"
                );
            } else if self.wait_ready(port).await {
                info!(pod = id, port, "pod ready");
                {
                    let mut meta = self.meta.lock().await;
                    meta.insert(id, PodMeta { port });
                }
                self.publish_backends().await;
            } else {
                // Readiness missed its initial window (slow start: cold DB
                // recovery, slow imports, …) but the pod process is still
                // alive. Previously the pod was then NEVER registered — the
                // sticky proxy kept an empty ring and reset every
                // connection (the node-3 8421 outage) until malkuth itself
                // restarted. Keep probing in a race against pod exit and
                // restart requests: the moment the port answers, register;
                // if the pod dies first, fall through to the respawn path;
                // if a rolling restart is requested meanwhile, drain and
                // respawn immediately (the notification would otherwise
                // wait for a consumer that is parked in this loop).
                warn!(
                    pod = id,
                    port, "pod did not become ready in time; continuing to probe while it lives"
                );
                let mut late_ready = false;
                let mut first_probe = true;
                loop {
                    if !first_probe {
                        // Pause between probes — raced against restart and
                        // exit so a request arriving during the pause is
                        // honored promptly, not after the full pause.
                        tokio::select! {
                            biased;
                            _ = notify.notified() => {
                                restart_now = true;
                                break;
                            },
                            _ = sleep(Duration::from_secs(2)) => {},
                            status = child.wait() => {
                                warn!(pod = id, ?status, "pod exited before becoming ready");
                                pod_alive = false;
                                break;
                            },
                        }
                    }
                    first_probe = false;
                    tokio::select! {
                        biased;
                        _ = notify.notified() => {
                            restart_now = true;
                            break;
                        },
                        ready = self.probe_ready(port) => {
                            if ready {
                                late_ready = true;
                                break;
                            }
                        },
                        status = child.wait() => {
                            warn!(pod = id, ?status, "pod exited before becoming ready");
                            pod_alive = false;
                            break;
                        },
                    }
                }
                if late_ready {
                    info!(
                        pod = id,
                        port, "pod became ready late; registering with proxy"
                    );
                    {
                        let mut meta = self.meta.lock().await;
                        meta.insert(id, PodMeta { port });
                    }
                    self.publish_backends().await;
                }
            }

            // Wait for either a natural exit or an explicit restart request.
            // A restart requested while readiness was being probed (or a pod
            // that already exited during that probing) must skip the wait
            // entirely — the notification permit is consumed, not parked.
            if restart_now {
                info!(
                    pod = id,
                    "draining pod for restart (requested during readiness probing)"
                );
                let _ = child.start_kill();
                let _ =
                    tokio::time::timeout(Duration::from_secs(self.drain_secs.max(1)), child.wait())
                        .await;
            } else if pod_alive {
                tokio::select! {
                    status = child.wait() => {
                        warn!(pod = id, ?status, "pod exited");
                    }
                    _ = notify.notified() => {
                        info!(pod = id, "draining pod for restart");
                        let _ = child.start_kill();
                        let _ = tokio::time::timeout(
                            Duration::from_secs(self.drain_secs.max(1)),
                            child.wait(),
                        )
                        .await;
                    }
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
        cmd.stdout(Stdio::piped());
        cmd.stderr(Stdio::piped());
        cmd.kill_on_drop(true);
        info!(pod = id, port, program, "spawning pod");
        let mut child = cmd.spawn()?;

        let max_lines = 500usize;
        if let Some(stdout) = child.stdout.take() {
            let log = Arc::clone(&self.runtime_log);
            tokio::spawn(async move {
                let mut lines = BufReader::new(stdout).lines();
                while let Ok(Some(line)) = lines.next_line().await {
                    let t = line.trim().to_string();
                    if !t.is_empty() {
                        if let Ok(mut g) = log.lock() {
                            g.push(t);
                            if g.len() > max_lines {
                                g.remove(0);
                            }
                        }
                    }
                }
            });
        }

        if let Some(stderr) = child.stderr.take() {
            let log = Arc::clone(&self.runtime_log);
            tokio::spawn(async move {
                let mut lines = BufReader::new(stderr).lines();
                while let Ok(Some(line)) = lines.next_line().await {
                    let t = line.trim().to_string();
                    if !t.is_empty() {
                        if let Ok(mut g) = log.lock() {
                            g.push(t);
                            if g.len() > max_lines {
                                g.remove(0);
                            }
                        }
                    }
                }
            });
        }

        Ok(child)
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

    /// Single readiness probe (one TCP connect attempt). Used by the
    /// late-readiness loop after the initial window is missed.
    async fn probe_ready(&self, port: u16) -> bool {
        let addr: SocketAddr = format!("{}:{}", self.host, port)
            .parse()
            .unwrap_or_else(|_| format!("127.0.0.1:{port}").parse().unwrap());
        TcpStream::connect(addr).await.is_ok()
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

    /// A marker line the fake pod prints on every spawn: `SPAWNED-<pid>`.
    /// The pool pipes child stdout into `runtime_log`, so counting distinct
    /// markers observes real child replacement.
    const MARKER_PREFIX: &str = "SPAWNED-";

    fn marker_pids(log: &Arc<StdMutex<Vec<String>>>) -> Vec<u32> {
        log.lock()
            .unwrap()
            .iter()
            .filter_map(|l| l.strip_prefix(MARKER_PREFIX)?.parse().ok())
            .collect()
    }

    async fn wait_for_two_spawns(log: &Arc<StdMutex<Vec<String>>>) -> bool {
        // restart_one must drain + respawn within a few seconds (drain 1s
        // + backoff 150ms + spawn); without the fix the notification is
        // never consumed and a second spawn never happens. 10s is generous.
        for _ in 0..100 {
            if marker_pids(log).len() >= 2 {
                return true;
            }
            sleep(Duration::from_millis(100)).await;
        }
        false
    }

    fn fake_pod_command() -> Vec<String> {
        vec![
            "/bin/sh".into(),
            "-c".into(),
            format!("echo {MARKER_PREFIX}$$; exec sleep 60"),
        ]
    }

    fn manager(
        port: u16,
        log: Arc<StdMutex<Vec<String>>>,
        readiness_timeout: Option<Duration>,
    ) -> Arc<PodManager> {
        let mgr = PodManager::new(
            "127.0.0.1".into(),
            "PORT".into(),
            fake_pod_command(),
            None,
            HashMap::from([(0, port)]),
            1,
            log,
        );
        let mgr = match readiness_timeout {
            Some(d) => mgr.with_readiness_timeout(d),
            None => mgr,
        };
        Arc::new(mgr)
    }

    /// Regression (--serve-only, no --proxy → port 0): a rolling restart
    /// requested by the watcher must replace the child even though no
    /// backend port exists to probe. Before the fix the supervision task
    /// parked forever in the late-readiness probe loop (connect to port 0
    /// never succeeds) and the restart notification went unconsumed — the
    /// node-2 evernight-server-malkuth incident.
    #[tokio::test]
    async fn restart_replaces_child_without_proxy_port() {
        let log = Arc::new(StdMutex::new(Vec::new()));
        let mgr = manager(0, Arc::clone(&log), None);
        mgr.clone().run().await;

        // First spawn must appear promptly.
        for _ in 0..100 {
            if !marker_pids(&log).is_empty() {
                break;
            }
            sleep(Duration::from_millis(100)).await;
        }
        assert_eq!(marker_pids(&log).len(), 1, "first spawn missing");

        mgr.restart_one(0).await;
        assert!(
            wait_for_two_spawns(&log).await,
            "restart without --proxy never respawned the pod (stuck probe loop)"
        );
        let pids = marker_pids(&log);
        assert_eq!(pids.len(), 2);
        assert_ne!(pids[0], pids[1], "restart must spawn a new process");
    }

    /// Regression (slow readiness): a restart requested while the pool is
    /// still probing a not-yet-ready port must be honored instead of being
    /// parked until the pod happens to become ready or die.
    #[tokio::test]
    async fn restart_during_readiness_probing_is_honored() {
        let log = Arc::new(StdMutex::new(Vec::new()));
        // A high port nothing listens on: initial window short so we land
        // in the late-probe loop quickly.
        let mgr = manager(0xF00D, Arc::clone(&log), Some(Duration::from_millis(300)));
        mgr.clone().run().await;

        for _ in 0..100 {
            if !marker_pids(&log).is_empty() {
                break;
            }
            sleep(Duration::from_millis(100)).await;
        }
        assert_eq!(marker_pids(&log).len(), 1, "first spawn missing");

        // Still inside the probe window/loop when the request arrives.
        sleep(Duration::from_millis(700)).await;
        mgr.restart_one(0).await;
        assert!(
            wait_for_two_spawns(&log).await,
            "restart during readiness probing never respawned the pod"
        );
    }
}
