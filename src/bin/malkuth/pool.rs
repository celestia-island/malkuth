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
    future::Future,
    net::SocketAddr,
    process::Stdio,
    sync::{Arc, Mutex as StdMutex},
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

/// Audit malkuth-R1-1: stop a managed pod with a drain escalation
/// chain — SIGTERM first so the service can finish in-flight work,
/// wait up to `drain_secs`, then SIGKILL and reap. The old call sites
/// started with SIGKILL, which made `drain_secs` a meaningless wait
/// cap and dropped in-flight requests on every restart.
async fn stop_draining(child: &mut tokio::process::Child, drain_secs: u64) {
    if let Some(pid) = child.id() {
        send_graceful(pid);
    }
    match tokio::time::timeout(
        std::time::Duration::from_secs(drain_secs.max(1)),
        child.wait(),
    )
    .await
    {
        // Exited within the drain window (gracefully or not).
        Ok(_) => {}
        Err(_) => {
            // Failure means the child already exited; the reap below
            // tolerates that.
            let _ = child.start_kill();
            let _ = child.wait().await;
        }
    }
}

#[cfg(unix)]
fn send_graceful(pid: u32) {
    // SAFETY: plain signal send to a live child pid; no memory ops.
    unsafe {
        libc::kill(pid as libc::pid_t, libc::SIGTERM);
    }
}

#[cfg(windows)]
fn send_graceful(pid: u32) {
    use windows_sys::Win32::System::Console::{CTRL_BREAK_EVENT, GenerateConsoleCtrlEvent};
    // Best-effort CTRL_BREAK; services have no console, so failure is
    // expected and the drain timeout still escalates to TerminateProcess.
    unsafe {
        GenerateConsoleCtrlEvent(CTRL_BREAK_EVENT, pid);
    }
}

/// Poll `/health` every 5 s while waiting for in-flight turns to finish.
const BUSY_PROBE_INTERVAL: Duration = Duration::from_secs(5);
/// Per-request HTTP timeout for one `/health` probe.
const BUSY_PROBE_HTTP_TIMEOUT: Duration = Duration::from_secs(2);
/// While still busy, emit a progress line at least this often.
const BUSY_WAIT_PROGRESS_EVERY: Duration = Duration::from_secs(60);

/// One observation of the pod's busy signal (design
/// `_reports/chain-resume-cb-design-20261111` §1.1: busy = at least one
/// user turn currently being processed, as reported by scepter's
/// `in_flight_turns` semaphore count).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum ProbeOutcome {
    /// `/health` reported a positive in-flight turn count.
    Busy(u64),
    /// Idle: zero in-flight turns — or any probe failure shape mapped to
    /// idle by design (fail-open, DESIGN §1.5).
    Idle,
}

/// Why the busy wait ended.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum WaitReason {
    /// The pod went idle (or every probe failed — fail-open).
    Idle,
    /// The pod kept reporting in-flight turns until the cap elapsed; the
    /// restart proceeds anyway (`restart_proceeding_busy_expired`).
    CapExpired,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
struct WaitSummary {
    /// Time spent waiting (virtual under tests).
    waited: Duration,
    /// Last observed in-flight turn count; `None` when never busy.
    last_in_flight: Option<u64>,
    reason: WaitReason,
}

/// Fetcher + clock seam for [`wait_until_idle_with`]. Production wires HTTP
/// GETs to the pod's `/health` and real sleeps; tests inject scripted
/// outcomes and a virtual clock so the wait semantics are pinned without
/// network or real time.
trait BusyWaitIo {
    fn fetch(&mut self) -> impl Future<Output = ProbeOutcome>;
    fn pause(&mut self, step: Duration) -> impl Future<Output = ()>;
}

/// Production IO for [`wait_until_idle_with`].
struct HealthHttp {
    client: reqwest::Client,
    url: String,
}

impl BusyWaitIo for HealthHttp {
    async fn fetch(&mut self) -> ProbeOutcome {
        probe_in_flight(&self.client, &self.url).await
    }

    async fn pause(&mut self, step: Duration) {
        sleep(step).await;
    }
}

/// One `GET /health` observation. Fail-open by construction: only a 2xx
/// response whose body is JSON carrying a positive integer
/// `in_flight_turns` counts as busy; connection refused, timeout, non-2xx,
/// non-JSON body and missing/malformed fields all return [`ProbeOutcome::Idle`].
async fn probe_in_flight(client: &reqwest::Client, url: &str) -> ProbeOutcome {
    let response = match client.get(url).send().await {
        Ok(response) => response,
        Err(_) => return ProbeOutcome::Idle,
    };
    if !response.status().is_success() {
        return ProbeOutcome::Idle;
    }
    match response.text().await {
        Ok(body) => classify_health_body(Some(&body)),
        Err(_) => classify_health_body(None),
    }
}

/// Pure classifier for a `/health` body (`None` = body unreadable). Kept
/// free of IO so the fail-open mapping is unit-testable without HTTP.
fn classify_health_body(body: Option<&str>) -> ProbeOutcome {
    let Some(body) = body else {
        return ProbeOutcome::Idle;
    };
    let Ok(value) = serde_json::from_str::<serde_json::Value>(body) else {
        return ProbeOutcome::Idle;
    };
    match value.get("in_flight_turns").and_then(|v| v.as_u64()) {
        Some(n) if n > 0 => ProbeOutcome::Busy(n),
        _ => ProbeOutcome::Idle,
    }
}

/// Bounded busy wait before a drain (DESIGN §1.4/§1.5). Polls `probe.fetch()`
/// every `interval`; a busy observation delays the restart, an idle one — or
/// any probe failure, which the fetcher maps to idle — ends the wait at once.
/// The total wait never exceeds `cap`: the final pause is clamped so the
/// wait lands exactly on the cap, then the restart proceeds regardless
/// (fail-open beats blocking a rollout). `cap == 0` disables probing
/// entirely (`--busy-wait-secs 0` = today's behavior).
async fn wait_until_idle_with<P: BusyWaitIo>(
    probe: &mut P,
    pod: usize,
    cap: Duration,
    interval: Duration,
) -> WaitSummary {
    if cap.is_zero() {
        return WaitSummary {
            waited: Duration::ZERO,
            last_in_flight: None,
            reason: WaitReason::Idle,
        };
    }
    let mut waited = Duration::ZERO;
    let mut last_in_flight = None;
    let mut progress_logged_at = Duration::ZERO;
    let mut announced = false;
    loop {
        // Cap check first: never probe once the budget is spent, and never
        // wait past the cap (the final step is clamped below).
        if waited >= cap {
            return WaitSummary {
                waited,
                last_in_flight,
                reason: WaitReason::CapExpired,
            };
        }
        match probe.fetch().await {
            ProbeOutcome::Idle => {
                // Fail-open: an idle reading — including every probe failure
                // shape the fetcher maps to idle — never delays the restart.
                return WaitSummary {
                    waited,
                    last_in_flight,
                    reason: WaitReason::Idle,
                };
            }
            ProbeOutcome::Busy(n) => {
                if !announced {
                    info!(
                        pod,
                        event = "restart_delayed_inflight",
                        in_flight_turns = n,
                        "pod still has in-flight turns; delaying drain"
                    );
                    announced = true;
                }
                last_in_flight = Some(n);
            }
        }
        // Clamp the final step so the total wait is exactly `cap`.
        let step = interval.min(cap - waited);
        probe.pause(step).await;
        waited += step;
        if waited - progress_logged_at >= BUSY_WAIT_PROGRESS_EVERY {
            info!(
                pod,
                event = "restart_wait_inflight_progress",
                waited_secs = waited.as_secs(),
                in_flight_turns = last_in_flight.unwrap_or(0),
                "still waiting for in-flight turns to finish"
            );
            progress_logged_at = waited;
        }
    }
}

/// `http://<host>:<port>/health` for a pod on `port`. Addresses the pod
/// exactly like the readiness TCP probe ([`PodManager::wait_ready`]) so a pod
/// bound to a non-default `--host` is still reached; an unparseable host falls
/// back to loopback the same way.
fn health_url(host: &str, port: u16) -> String {
    let addr: SocketAddr = format!("{host}:{port}")
        .parse()
        .unwrap_or_else(|_| format!("127.0.0.1:{port}").parse().unwrap());
    format!("http://{addr}/health")
}

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
    /// Upper bound on the pre-drain busy wait (`--busy-wait-secs`; 0 disables
    /// the probe entirely).
    busy_wait_secs: u64,
    /// Shared client for `/health` busy probes, built once and reused by every
    /// pod's supervision task. `None` when the build failed: the busy wait is
    /// then skipped outright (fail-open — a probe-side failure must never
    /// block a rolling restart).
    busy_probe_client: Option<reqwest::Client>,
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
        busy_wait_secs: u64,
        runtime_log: Arc<StdMutex<Vec<String>>>,
    ) -> Self {
        let restart = ports
            .keys()
            .map(|id| (*id, Arc::new(Notify::new())))
            .collect();
        // Built once here (never inside `supervise`) and shared by every pod:
        // reqwest's client owns the connection pool, and rebuilding it per
        // restart would throw that away. `http_client_builder` is the crate's
        // canonical builder — it installs the process-wide rustls provider
        // (this binary is built with `rustls-no-provider`). A build failure
        // disables only the busy wait; rolling restarts proceed (fail-open).
        let busy_probe_client = match malkuth::http_client_builder()
            .timeout(BUSY_PROBE_HTTP_TIMEOUT)
            .build()
        {
            Ok(client) => Some(client),
            Err(e) => {
                warn!(
                    error = %e,
                    "failed to build the /health probe client; busy wait disabled (fail-open)"
                );
                None
            }
        };
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
            busy_wait_secs,
            busy_probe_client,
            runtime_log,
        }
    }

    /// Override the initial readiness window (tests only; production keeps
    /// the 30s default).
    #[cfg(all(test, unix))]
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

    /// Hold the coming drain while the pod reports in-flight turns
    /// (DESIGN §1.3–§1.5). Returns at once when the pod reads idle, on any
    /// probe failure shape (fail-open), on `--busy-wait-secs 0`, and when the
    /// deployment has no backend port to probe (`--serve-only` assigns port
    /// 0). If the cap expires the restart proceeds anyway, after a warn: a
    /// stuck busy signal must never stall a rollout forever.
    ///
    /// Accepted by design (DESIGN §1.4/§1.5): the pod is still registered as
    /// a proxy backend for the whole wait, so a pod under sustained load keeps
    /// receiving new turns and `in_flight_turns` rarely reaches zero — such a
    /// rolling restart honestly pays the full cap (120 s per pod by default).
    /// That trade is the point of the feature (a slower rollout instead of
    /// dropped turns); `--busy-wait-secs 0` is the escape hatch.
    async fn wait_until_idle(&self, pod: usize, port: u16) {
        if port == 0 {
            // No --proxy: no backend port was assigned, so there is nothing
            // to probe (same reasoning as the readiness skip above).
            return;
        }
        let Some(client) = self.busy_probe_client.clone() else {
            return;
        };
        let mut probe = HealthHttp {
            client,
            url: health_url(&self.host, port),
        };
        // `busy_wait_secs` is a u64 (already non-negative); a zero cap is
        // short-circuited inside `wait_until_idle_with`.
        let cap = Duration::from_secs(self.busy_wait_secs);
        let summary = wait_until_idle_with(&mut probe, pod, cap, BUSY_PROBE_INTERVAL).await;
        if summary.reason == WaitReason::CapExpired {
            warn!(
                pod,
                event = "restart_proceeding_busy_expired",
                waited_secs = summary.waited.as_secs(),
                in_flight_turns = summary.last_in_flight.unwrap_or(0),
                "pod still reported in-flight turns at the busy-wait cap; restarting anyway"
            );
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
            //
            // Accepted by design (DESIGN §1.4/§1.5): the pod stays registered
            // as a proxy backend while `wait_until_idle` runs, so a pod under
            // sustained load keeps receiving new turns and `in_flight_turns`
            // rarely reaches zero — such a rollout honestly pays the full cap
            // per pod (120 s by default). Slower rollouts instead of dropped
            // turns is the documented trade; `--busy-wait-secs 0` opts out.
            // What would NOT be acceptable is a duplicate trigger parked
            // during that wait buying a second drain round afterwards, so the
            // permit is dropped as soon as the wait ends.
            if restart_now {
                self.wait_until_idle(id, port).await;
                discard_pending_restarts(&notify, id);
                info!(
                    pod = id,
                    "draining pod for restart (requested during readiness probing)"
                );
                stop_draining(&mut child, self.drain_secs).await;
            } else if pod_alive {
                tokio::select! {
                    status = child.wait() => {
                        warn!(pod = id, ?status, "pod exited");
                    }
                    _ = notify.notified() => {
                        self.wait_until_idle(id, port).await;
                        discard_pending_restarts(&notify, id);
                        info!(pod = id, "draining pod for restart");
                        stop_draining(&mut child, self.drain_secs).await;
                    }
                }
            }

            // Pod gone: also coalesce a restart request that landed during the
            // drain itself (bounded by `drain_secs`) — this respawn already
            // starts the newest binary, so a repeat request for this pod has
            // nothing left to do.
            discard_pending_restarts(&notify, id);

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
        #[cfg(windows)]
        {
            use std::os::windows::process::CommandExt;
            // CREATE_NEW_PROCESS_GROUP: makes CTRL_BREAK target this pod only.
            cmd.as_std_mut().creation_flags(0x0000_0200);
        }
        info!(pod = id, port, program, "spawning pod");
        let mut child = cmd.spawn()?;

        // #61 piped the child's stdio purely into the in-memory ring (VTTY
        // view) — the lines never reached tracing, so supervised pods had
        // zero journald visibility (2026-08-26 incident review). forward()
        // keeps the ring capture AND re-emits under the `malkuth::pod`
        // target, with rate-limited shedding that never stops draining.
        if let Some(stdout) = child.stdout.take() {
            crate::log_forward::forward(stdout, id, "stdout", Arc::clone(&self.runtime_log));
        }
        if let Some(stderr) = child.stderr.take() {
            crate::log_forward::forward(stderr, id, "stderr", Arc::clone(&self.runtime_log));
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

/// Coalesce a repeat rolling-restart request that arrived while this pod was
/// already being taken out of rotation (review S2-2 on PR #192).
///
/// [`PodManager::restart_one`] signals one [`Notify`] per pod, and `Notify`
/// parks at most one permit when nobody is waiting — exactly the state a
/// duplicate trigger leaves behind during the long busy wait
/// (`--busy-wait-secs`, 120 s by default). The respawned pod would consume
/// that parked permit on its first `select!` and pay a second, pointless drain
/// round ("多一次重启"). `Notified::enable` takes a parked permit without
/// awaiting, so the duplicate is dropped here instead.
///
/// This cannot swallow a real signal: the map is a restart-request channel
/// written only by [`PodManager::restart_one`]; shutdown/reload travel through
/// the signal handler, never through this `Notify`. A dropped request is also
/// already satisfied: the respawn that follows starts the pod from the newest
/// binary on disk, which is all a rolling-restart request asks for. Returns
/// whether a duplicate was actually discarded.
fn discard_pending_restarts(notify: &Notify, pod: usize) -> bool {
    let notified = notify.notified();
    tokio::pin!(notified);
    if !notified.as_mut().enable() {
        return false;
    }
    info!(
        pod,
        event = "restart_request_coalesced",
        "restart requested while this pod was already draining; the respawn covers it"
    );
    true
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

#[cfg(all(test, unix))]
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
            // The busy wait is unit-tested directly below; these supervision
            // tests keep it off so their timing stays about spawn/drain.
            0,
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

    /// `Notify` parks at most one permit when nobody is waiting, and that
    /// permit is exactly what a duplicate trigger leaves behind during a busy
    /// wait. Coalescing must take it without awaiting, and idempotently.
    #[tokio::test]
    async fn busy_wait_coalesces_the_parked_restart_permit() {
        let notify = Notify::new();
        assert!(!discard_pending_restarts(&notify, 0), "nothing parked yet");

        notify.notify_one();
        notify.notify_one();
        assert!(
            discard_pending_restarts(&notify, 0),
            "the parked permit must be discarded"
        );
        assert!(
            !discard_pending_restarts(&notify, 0),
            "a second call must find nothing: Notify holds at most one permit"
        );
        // Nothing stale is left over, so the next wait really has to wait.
        assert!(
            tokio::time::timeout(Duration::from_millis(50), notify.notified())
                .await
                .is_err(),
            "the discard must not leave a wakeup behind"
        );
    }

    /// Review S2-2: a repeat rolling-restart request that lands while the
    /// supervisor is already busy-waiting for in-flight turns must be
    /// coalesced. Before the fix it parked a permit that the respawned pod
    /// consumed immediately, paying a second, pointless drain round.
    #[tokio::test]
    async fn duplicate_trigger_during_busy_wait_adds_no_extra_drain() {
        use super::fake_pod;

        // Busy for the first health reading (the wait below), idle afterwards:
        // a coalesced duplicate cannot hide behind a second full cap — it would
        // drain the fresh pod at once and spawn a third child.
        let port = fake_pod::busy_then_idle(1).await;
        let log = Arc::new(StdMutex::new(Vec::new()));
        let mgr = Arc::new(PodManager::new(
            "127.0.0.1".into(),
            "PORT".into(),
            fake_pod_command(),
            None,
            HashMap::from([(0, port)]),
            1, // drain_secs
            2, // busy_wait_secs: one busy reading + a 2s clamped pause
            Arc::clone(&log),
        ));
        mgr.clone().run().await;

        for _ in 0..100 {
            if !marker_pids(&log).is_empty() {
                break;
            }
            sleep(Duration::from_millis(100)).await;
        }
        assert_eq!(marker_pids(&log).len(), 1, "first spawn missing");

        // Let readiness + registration finish so the trigger lands in the
        // supervision `select!`, then start the restart: the pod reports busy,
        // so the drain is held for the whole 2s cap.
        sleep(Duration::from_millis(300)).await;
        mgr.restart_one(0).await;
        sleep(Duration::from_millis(700)).await; // inside the busy wait
        mgr.restart_one(0).await; // duplicate trigger

        // The legitimate restart lands after the wait + drain + respawn. A
        // third spawn would mean the duplicate bought another drain round.
        let mut max_seen = 0;
        for _ in 0..120 {
            max_seen = max_seen.max(marker_pids(&log).len());
            if max_seen > 2 {
                break;
            }
            sleep(Duration::from_millis(50)).await;
        }
        assert_eq!(
            max_seen, 2,
            "a duplicate trigger during the busy wait must not add a drain round"
        );
    }
}

#[cfg(all(test, unix))]
mod stop_draining_tests {
    use super::*;
    use std::os::unix::process::ExitStatusExt;

    fn spawn_sh(script: &'static str) -> tokio::process::Child {
        let mut cmd = tokio::process::Command::new("sh");
        cmd.arg("-c").arg(script).kill_on_drop(true);
        cmd.spawn().expect("spawn sh")
    }

    /// Audit malkuth-R1-1: a service that honors SIGTERM exits within
    /// the drain window — no escalation needed.
    #[tokio::test]
    async fn sigterm_graceful_exit_within_drain_window() {
        let mut child = spawn_sh("sleep 30");
        let started = std::time::Instant::now();
        stop_draining(&mut child, 2).await;
        assert!(
            started.elapsed() < std::time::Duration::from_secs(2),
            "a SIGTERM-honoring child must exit without escalation"
        );
        let status = child.try_wait().expect("reap").expect("exited");
        assert!(
            status.signal() == Some(libc::SIGTERM),
            "the child must have died from SIGTERM, got {status:?}"
        );
    }

    /// Audit malkuth-R1-1: a child that IGNORES SIGTERM gets escalated
    /// to SIGKILL once the drain window elapses — stop_draining must
    /// always terminate the child.
    #[tokio::test]
    async fn sigterm_ignoring_child_is_escalated_to_sigkill() {
        // python3 holds SIG_IGN robustly (shell traps proved unreliable
        // across sh variants — the child died before the window elapsed).
        let mut child = {
            let mut cmd = tokio::process::Command::new("python3");
            cmd.arg("-c")
                .arg("import signal,time; signal.signal(signal.SIGTERM, signal.SIG_IGN); time.sleep(30)")
                .kill_on_drop(true);
            cmd.spawn().expect("spawn python3")
        };
        // Give the child time to install its SIG_IGN handler: a SIGTERM
        // landing during interpreter startup hits the default
        // disposition and kills it instantly (not the scenario under
        // test).
        tokio::time::sleep(std::time::Duration::from_millis(300)).await;
        let started = std::time::Instant::now();
        stop_draining(&mut child, 1).await;
        let elapsed = started.elapsed();
        assert!(
            elapsed >= std::time::Duration::from_millis(900),
            "the drain window must actually be honored before escalation"
        );
        assert!(
            elapsed < std::time::Duration::from_secs(5),
            "escalation must not hang: took {elapsed:?}"
        );
        let status = child.try_wait().expect("reap").expect("exited");
        assert!(
            status.signal() == Some(libc::SIGKILL),
            "the child must have been SIGKILLed, got {status:?}"
        );
    }
}

#[cfg(all(test, unix))]
mod fake_pod {
    //! Hand-rolled loopback HTTP pods for the wire-level probe tests
    //! (review S2-1). No axum and no test HTTP framework: a `TcpListener` on
    //! port 0 plus a few lines of request/response is enough to drive the
    //! real reqwest path, and it keeps the fake pod's answer shape — status
    //! line, headers, truncated bodies, silence — under the test's control.
    use super::*;
    use tokio::{
        io::{AsyncReadExt, AsyncWriteExt},
        net::TcpListener,
    };

    /// One HTTP/1.1 reply, `Connection: close` so the client dials afresh for
    /// the next probe (the pool reuses one client across pods).
    pub(super) fn reply(status: u16, body: &str) -> Vec<u8> {
        let reason = match status {
            200 => "OK",
            400 => "Bad Request",
            500 => "Internal Server Error",
            _ => "Unknown",
        };
        format!(
            "HTTP/1.1 {status} {reason}\r\nContent-Type: application/json\r\n\
             Content-Length: {}\r\nConnection: close\r\n\r\n{body}",
            body.len()
        )
        .into_bytes()
    }

    /// Read one request head. `false` means the peer closed without sending a
    /// byte — a readiness `TcpStream::connect` behaves exactly so.
    pub(super) async fn read_request_head(stream: &mut TcpStream) -> bool {
        let mut buf = Vec::new();
        let mut chunk = [0u8; 256];
        loop {
            match stream.read(&mut chunk).await {
                Ok(0) | Err(_) => return !buf.is_empty(),
                Ok(n) => {
                    buf.extend_from_slice(&chunk[..n]);
                    if buf.windows(4).any(|w| w == b"\r\n\r\n") || buf.len() > 8192 {
                        return true;
                    }
                }
            }
        }
    }

    /// Serve one scripted raw response per accepted request, then stop.
    pub(super) async fn scripted(replies: Vec<Vec<u8>>) -> u16 {
        let listener = TcpListener::bind(("127.0.0.1", 0))
            .await
            .expect("bind fake pod");
        let port = listener.local_addr().expect("fake pod addr").port();
        tokio::spawn(async move {
            for reply in replies {
                let Ok((mut stream, _)) = listener.accept().await else {
                    return;
                };
                let _ = read_request_head(&mut stream).await;
                let _ = stream.write_all(&reply).await;
            }
        });
        port
    }

    /// Answer `/health` with in-flight turns for the first `busy_rounds`
    /// requests, idle afterwards; accept readiness connects too (they send no
    /// bytes, so there is nothing to answer).
    pub(super) async fn busy_then_idle(busy_rounds: usize) -> u16 {
        let listener = TcpListener::bind(("127.0.0.1", 0))
            .await
            .expect("bind fake pod");
        let port = listener.local_addr().expect("fake pod addr").port();
        let seen = Arc::new(std::sync::atomic::AtomicUsize::new(0));
        tokio::spawn(async move {
            loop {
                let Ok((mut stream, _)) = listener.accept().await else {
                    return;
                };
                let seen = Arc::clone(&seen);
                tokio::spawn(async move {
                    if !read_request_head(&mut stream).await {
                        return;
                    }
                    let busy = seen.fetch_add(1, std::sync::atomic::Ordering::SeqCst) < busy_rounds;
                    let reply = if busy {
                        reply(200, r#"{"in_flight_turns":3}"#)
                    } else {
                        reply(200, r#"{"in_flight_turns":0}"#)
                    };
                    let _ = stream.write_all(&reply).await;
                });
            }
        });
        port
    }
}

#[cfg(all(test, unix))]
mod busy_wait_tests {
    use super::*;
    use std::collections::VecDeque;

    /// Scripted [`BusyWaitIo`]: serves a fixed outcome sequence and records
    /// every pause instead of sleeping — the wait semantics are pinned
    /// without network IO or real time.
    struct ScriptedProbe {
        outcomes: VecDeque<ProbeOutcome>,
        /// Served once the script is exhausted, so a regression that probes
        /// more than expected still terminates instead of panicking.
        tail: ProbeOutcome,
        fetches: usize,
        steps: Vec<Duration>,
    }

    impl ScriptedProbe {
        fn scripted(outcomes: impl IntoIterator<Item = ProbeOutcome>) -> Self {
            Self {
                outcomes: outcomes.into_iter().collect(),
                tail: ProbeOutcome::Idle,
                fetches: 0,
                steps: Vec::new(),
            }
        }

        fn always(outcome: ProbeOutcome) -> Self {
            let mut probe = Self::scripted(std::iter::empty());
            probe.tail = outcome;
            probe
        }
    }

    impl BusyWaitIo for ScriptedProbe {
        async fn fetch(&mut self) -> ProbeOutcome {
            self.fetches += 1;
            self.outcomes.pop_front().unwrap_or(self.tail)
        }

        async fn pause(&mut self, step: Duration) {
            self.steps.push(step);
        }
    }

    /// Busy once, then idle: the wait ends on the first idle reading and
    /// charges exactly the one 5s pause that followed the busy reading.
    #[tokio::test]
    async fn busy_then_idle_returns_immediately() {
        let mut probe = ScriptedProbe::scripted([ProbeOutcome::Busy(2), ProbeOutcome::Idle]);
        let summary = wait_until_idle_with(
            &mut probe,
            0,
            Duration::from_secs(120),
            Duration::from_secs(5),
        )
        .await;
        assert_eq!(summary.reason, WaitReason::Idle);
        assert_eq!(
            summary.waited,
            Duration::from_secs(5),
            "only the pause charged to the busy reading counts"
        );
        assert_eq!(summary.last_in_flight, Some(2));
        assert_eq!(probe.fetches, 2, "idle ends the wait at the second fetch");
        assert_eq!(probe.steps, vec![Duration::from_secs(5)]);
    }

    /// Busy until the cap: 12s budget at a 5s interval must land on exactly
    /// 12s (5 + 5 + clamped 2), then hand the restart back to the caller.
    #[tokio::test]
    async fn always_busy_stops_exactly_at_cap() {
        let mut probe = ScriptedProbe::always(ProbeOutcome::Busy(1));
        let summary = wait_until_idle_with(
            &mut probe,
            3,
            Duration::from_secs(12),
            Duration::from_secs(5),
        )
        .await;
        assert_eq!(summary.reason, WaitReason::CapExpired);
        assert_eq!(summary.waited, Duration::from_secs(12));
        assert_eq!(summary.last_in_flight, Some(1));
        assert_eq!(probe.fetches, 3);
        assert_eq!(
            probe.steps,
            vec![
                Duration::from_secs(5),
                Duration::from_secs(5),
                Duration::from_secs(2),
            ],
            "the final step is clamped to the remaining budget"
        );
        assert_eq!(
            probe.steps.iter().copied().sum::<Duration>(),
            summary.waited,
            "pauses must account for the whole wait"
        );
    }

    /// Fail-open (DESIGN §1.5): every failure shape the fetcher can produce
    /// — classified by the real classifier, exactly as `probe_in_flight`
    /// does — must end the wait at once instead of burning the cap.
    #[tokio::test]
    async fn probe_failures_are_fail_open() {
        let failure_shapes: [Option<&str>; 5] = [
            // Idle by value: a `n >= 0` regression would read this as busy.
            Some(r#"{"in_flight_turns":0}"#),
            // Unreadable body: timeout / truncated read.
            None,
            // Non-2xx / non-JSON error page.
            Some("<html>502 Bad Gateway</html>"),
            // Missing field on an older engine.
            Some("{}"),
            // Right field, wrong JSON type.
            Some(r#"{"in_flight_turns":"3"}"#),
        ];
        for body in failure_shapes {
            let mut probe = ScriptedProbe::scripted([classify_health_body(body)]);
            let summary = wait_until_idle_with(
                &mut probe,
                0,
                Duration::from_secs(120),
                Duration::from_secs(5),
            )
            .await;
            assert_eq!(
                summary.reason,
                WaitReason::Idle,
                "shape {body:?} must fail open"
            );
            assert_eq!(
                summary.waited,
                Duration::ZERO,
                "shape {body:?} must never delay the restart"
            );
            assert_eq!(summary.last_in_flight, None, "shape {body:?}");
            assert_eq!(probe.fetches, 1, "shape {body:?}: one reading, then out");
            assert!(
                probe.steps.is_empty(),
                "shape {body:?}: no pause may be charged"
            );
        }
    }

    /// `--busy-wait-secs 0` is the escape hatch: not a single probe.
    #[tokio::test]
    async fn zero_cap_never_probes() {
        let mut probe = ScriptedProbe::always(ProbeOutcome::Busy(9));
        let summary =
            wait_until_idle_with(&mut probe, 0, Duration::ZERO, Duration::from_secs(5)).await;
        assert_eq!(summary.reason, WaitReason::Idle);
        assert_eq!(summary.waited, Duration::ZERO);
        assert_eq!(summary.last_in_flight, None);
        assert_eq!(probe.fetches, 0, "a zero cap must not issue any probe");
        assert!(probe.steps.is_empty());
    }

    /// Only a positive integer count is busy; zero is idle (the semaphore is
    /// free), and a missing or non-integer field fails open.
    #[test]
    fn classify_health_body_only_positive_integer_is_busy() {
        assert_eq!(
            classify_health_body(Some(r#"{"in_flight_turns":3}"#)),
            ProbeOutcome::Busy(3)
        );
        assert_eq!(
            classify_health_body(Some(r#"{"in_flight_turns":0}"#)),
            ProbeOutcome::Idle
        );
        assert_eq!(
            classify_health_body(Some(r#"{"in_flight_turns":1}"#)),
            ProbeOutcome::Busy(1)
        );
    }

    /// The fail-open table on its own: unreadable body, empty/non-JSON body,
    /// missing field, wrong JSON type, negative count and unrelated JSON all
    /// read as idle.
    #[test]
    fn classify_health_body_failure_shapes_are_idle() {
        for body in [
            None,
            Some(""),
            Some("not json"),
            Some("{}"),
            Some(r#"{"in_flight_turns":"3"}"#),
            Some(r#"{"in_flight_turns":-1}"#),
            Some(r#"{"other":3}"#),
        ] {
            assert_eq!(classify_health_body(body), ProbeOutcome::Idle, "{body:?}");
        }
    }

    /// The probe must address the pod exactly like the readiness TCP probe:
    /// the same host plus the assigned backend port (`--host` defaults to
    /// loopback).
    #[test]
    fn health_url_targets_the_assigned_backend_port() {
        assert_eq!(
            health_url("127.0.0.1", 3001),
            "http://127.0.0.1:3001/health"
        );
        assert_eq!(health_url("0.0.0.0", 4100), "http://0.0.0.0:4100/health");
    }

    // ── The wire half of the fail-open table (review S2-1) ────────────
    //
    // The design asked for a "fake pod over TCP"; the wait itself was built
    // against a `BusyWaitIo` seam, which leaves `probe_in_flight` — the
    // status/body/timeout mapping — with no coverage at all. Both layers now
    // exist on purpose (PR #192, deviation ④): the seam pins the wait
    // arithmetic without IO, and these tests pin the HTTP shapes only the wire
    // can produce.

    fn probe_client(timeout: Duration) -> reqwest::Client {
        malkuth::http_client_builder()
            .timeout(timeout)
            .build()
            .expect("build probe client")
    }

    /// The real `probe_in_flight` over a real loopback socket: only a 2xx
    /// response whose body carries a positive integer count reads busy. The
    /// non-2xx case deliberately puts a busy-looking body behind a 500 so the
    /// status gate — not the body — decides; a regression that returns busy
    /// from the non-2xx branch turns this red.
    #[tokio::test]
    async fn probe_in_flight_over_tcp_maps_every_http_shape() {
        use super::fake_pod;

        let client = probe_client(Duration::from_secs(5));
        let cases = [
            (
                "busy body",
                fake_pod::reply(200, r#"{"in_flight_turns":3}"#),
                ProbeOutcome::Busy(3),
            ),
            (
                "idle body",
                fake_pod::reply(200, r#"{"in_flight_turns":0}"#),
                ProbeOutcome::Idle,
            ),
            (
                "non-2xx with a busy body",
                fake_pod::reply(500, r#"{"in_flight_turns":3}"#),
                ProbeOutcome::Idle,
            ),
            (
                "non-JSON body",
                fake_pod::reply(200, "<html>502 Bad Gateway</html>"),
                ProbeOutcome::Idle,
            ),
        ];
        for (label, reply, expected) in cases {
            let port = fake_pod::scripted(vec![reply]).await;
            let got = probe_in_flight(&client, &format!("http://127.0.0.1:{port}/health")).await;
            assert_eq!(got, expected, "{label}");
        }
    }

    /// A 200 whose announced body never completes (the pod dies mid-response)
    /// must fail open on the body read, not on the classifier.
    #[tokio::test]
    async fn probe_in_flight_unreadable_body_is_idle() {
        use super::fake_pod;

        let mut truncated =
            b"HTTP/1.1 200 OK\r\nContent-Type: application/json\r\nContent-Length: 64\r\n\
              Connection: close\r\n\r\n"
                .to_vec();
        truncated.extend_from_slice(br#"{"in_flight_turns":3}"#);
        let port = fake_pod::scripted(vec![truncated]).await;
        let client = probe_client(Duration::from_secs(5));
        let got = probe_in_flight(&client, &format!("http://127.0.0.1:{port}/health")).await;
        assert_eq!(got, ProbeOutcome::Idle, "an incomplete body must fail open");
    }

    /// A pod that accepts the connection and never answers must not pin the
    /// supervisor: reqwest surfaces the timeout as `Err`, read as idle.
    #[tokio::test]
    async fn probe_in_flight_times_out_to_idle() {
        use tokio::{io::AsyncReadExt, net::TcpListener};

        let listener = TcpListener::bind(("127.0.0.1", 0))
            .await
            .expect("bind silent pod");
        let port = listener.local_addr().expect("silent pod addr").port();
        let silent = tokio::spawn(async move {
            let Ok((mut stream, _)) = listener.accept().await else {
                return;
            };
            // Read the request, then hold the socket open without replying.
            let mut chunk = [0u8; 256];
            let _ = stream.read(&mut chunk).await;
            sleep(Duration::from_secs(30)).await;
        });
        let client = probe_client(Duration::from_millis(50));
        let started = Instant::now();
        let got = probe_in_flight(&client, &format!("http://127.0.0.1:{port}/health")).await;
        assert_eq!(got, ProbeOutcome::Idle, "a timeout must fail open");
        assert!(
            started.elapsed() < Duration::from_secs(5),
            "the probe must honor its own timeout, took {:?}",
            started.elapsed()
        );
        silent.abort();
    }
}
