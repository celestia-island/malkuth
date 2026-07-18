# Unified Supervision, Rolling Update & Replication Architecture
> **Scope.** This is a *platform-level* design: it cuts across `core`
> (entelecheia / scepter), `webui` (shittim-chest / chest) and `router`
> (evernight). Per-project architecture docs live under their own
> `core/`, `webui/`, `router/` subcategories; this document defines the
> shared lifecycle / supervision layer they all consume.
## 1. Background & Goals
The three projects are structurally homogeneous — all **Rust (edition
2024, MSRV 1.85) + axum 0.8 + tokio + JSON-RPC over Unix sockets /
WebSocket**, and all already share the `plana` crate as their protocol
layer. This homogeneity is what makes a *single* supervision mechanism
worth building once and reusing three times.
The mechanism must serve four overlapping needs expressed as one
coherent facility:
1. **Load balancing** — run multiple identical instances of the same
   program so they share the work, coordinating over IPC, while still
   sharing database / config / runtime state.
2. **Coordinated writes** — when one instance is about to write a shared
   file, it must notify the others and take a lock, so concurrent
   mutations do not corrupt state.
3. **Rolling updates** — when a new official release (or a freshly
   compiled local debug server) arrives, the new and old binaries can
   coexist; the old process finishes its in-flight work, then exits and
   hands the running set over to the new process.
4. **Edge fault tolerance** — on a device (notably evernight gateways)
   two processes can run as leader/follower so that a crash of one does
   not take the whole device down.
### 1.1 Current state (the gap this closes)
A code audit found the same three deficiencies in **all three**
projects:
| Capability | entelecheia (scepter) | shittim-chest (chest) | evernight |
|---|---|---|---|
| Signal handling | `ctrl_c` only (`shutdown.rs:17`) | `ctrl_c` only (`api.rs:465`) | `ctrl_c` only (`api/mod.rs:109`) |
| Drain logic | drains HTTP, not WS / bg tasks | same | none |
| Listener fd passing | none | none | none |
| `/readyz` with drain bit | none | has `/api/health`, no drain bit | none |
The headline problem: because only `SIGINT` is caught, `docker stop` /
`systemctl restart` — which send **`SIGTERM`** — bypass graceful
shutdown entirely and hard-kill after the grace period. Fixing this
alone is the single highest-leverage change.
### 1.2 Existing assets to reuse
- `entelecheia/packages/cli/src/evernight_daemon.rs` — the most complete
  self-restart blueprint today: PID lockfile + self-reexec + readiness
  wait + `SIGTERM`→`SIGKILL` fallback.
- `entelecheia/packages/scepter/src/daemon/health_daemon.rs` — a
  **JSON-manifest file queue** that performs container rolling updates
  (a language-agnostic update primitive).
- `entelecheia/packages/shared/infra_jsonrpc` — the Unix-socket
  JSON-RPC transport layer.
- `shittim-chest/packages/core/src/proxy/upstream_pool.rs` — a
  multi-endpoint registry with exponential-backoff reconnect; a ready
  template for a load-balanced client.
- `evernight/src/model_server.rs` — a `Running/Starting/Stopped/Failed`
  resource lifecycle state machine with deploy→wait-health→stop-old;
  the in-repo template for rolling update at the application layer.
## 2. Theoretical Basis
The mechanism is not one theory but a composition of well-established
industrial patterns, each with canonical implementations. Naming them
explicitly so the design inherits their proven semantics:
| Need expressed | Industrial term | Canonical implementation |
|---|---|---|
| New & old processes coexist; old finishes in-flight then exits | **Graceful shutdown / drain** + **rolling update** | Kubernetes Deployment, nginx / unicorn |
| "Red/blue marker" (the name recalled in discussion) | **Blue-Green deployment** (two parallel envs, switch traffic pointer). The progressive-decay behaviour described is closer to **rolling update + drain** than to blue-green. | |
| New process takes over the same port without dropping connections | **socket activation / fd inheritance / `SO_REUSEPORT`** | systemd, nginx `USR2`, envoy hot restart |
| "Notify the other and lock before writing" | **advisory lock (`flock`/`fcntl`) / DB row lock / lease** | POSIX advisory locks, `pg_advisory_lock` |
| Process self-healing | **supervision tree (OTP) / systemd / kubelet** | Erlang/OTP, systemd, s6, immortal |
| Leader/follower so a crash does not kill the service | **lease-based leader election + fencing** | Chubby lease, Raft leader election (subset), keepalived/VRRP, Pacemaker |
| Resource held by one process, restarted on crash | **"Let it crash" + supervisor restart** | Erlang/OTP supervision trees, systemd `Restart=always` |
**The industrial standard recipe for rolling update** (worth copying
verbatim) is Kubernetes':
`maxSurge + maxUnavailable + readinessProbe + preStop hook + SIGTERM
graceful shutdown + grace period + PodDisruptionBudget`. What we build
is the self-hosted, single-host / small-cluster variant of that recipe.
**nginx hot-upgrade** is the textbook for "new & old coexist, then old
drains": `USR2` starts a new master that inherits the listen fd → `WINCH`
gracefully stops the old workers → `QUIT` retires the old master. The
flow in §7 is structurally identical.
## 3. Overall Architecture
The design converges on a single elegant backbone: **a supervision
tree everywhere; the only difference is whether the supervisor is a
peer replica or a leader/follower.**
```
                    ┌─────────────────────────────────────────┐
   Shared base      │ signal semantics · drain               │  ← identical for
   (Layer 1 / 3)    │ /healthz · /readyz (with drain bit)    │     all three projects
                    │ socket activation + 3 deploy adapters   │
                    └─────────────────────────────────────────┘
                                   │
          ┌────────────────────────┴────────────────────────┐
          ▼                                                  ▼
   ┌────────────────┐                              ┌──────────────────┐
   │ Subsystem A    │  supervisors are PEERS       │ Subsystem B      │  supervisor is
   │ Replica        │  (active-active)             │ Leader/Follower  │  leader or follower
   │                │  ← server LB + rolling update│                  │  ← edge fault tolerance
   └───────┬────────┘                              └─────────┬────────┘
           │                                                  │
           └──────────────────┬───────────────────────────────┘
                              ▼
               ┌──────────────────────────────┐
               │ Unified Worker abstraction   │  ← every "child-process resource"
               │ lifecycle FSM + supervised   │     across all three projects
               │ restart (permanent/transient)│     cosmos / pglite-proxy / per-protocol workers
               │ + sliding-window rate limit  │
               └──────────────────────────────┘
```
### 3.1 The key simplification: rolling update is *not* a third system
Once the supervisor tree is the backbone, the user's needs collapse to
**two** subsystems, not three:
- **Subsystem A — Replica.** Server-side runs N identical peer
  instances that share load. *Load balancing and rolling update are the
  same subsystem:* rolling update is just "replica count temporarily +1
  → drain one old replica → repeat". It is the `maxSurge/maxUnavailable`
  operation of Kubernetes, expressed as replica add/remove.
- **Subsystem B — Leader/Follower.** Edge (evernight device) runs two
  processes, one leader one follower, for fault tolerance. The leader
  exclusively owns physical I/O; the follower waits.
There is no separate "rolling update system": it is a maintenance
operation of Subsystem A (and, with different semantics, of B via
leader failover).
### 3.2 Layered view
| Layer | Subsystem A (Replica) | Subsystem B (Leader/Follower) | Shared? |
|---|---|---|---|
| **L1** lifecycle (signals / drain / probes) | same | same | **shared** |
| **L3** zero-downtime handoff (socket activation) | each replica gets fd from systemd | leader→follower fd takeover (advanced) | **shared** |
| **L2** coordination | **2a** peer registry + shared lock (`pg_advisory`) | **2b** lease election + exclusive resource + leader/follower registry | **forks** (same trait, different policy) |
| **L4** orchestration | **4a** replica scale / rolling update | **4b** failover | **forks** |
Insight: **L1 and L3 are fully shared; L2/L4 fork.** `CoordinationLock`
is the same trait in 2a and 2b — used to coordinate concurrent writes
in A, used as the leader lease in B. That trait unification is precisely
where "the principles are common" lands.
## 4. Crate Ownership
The user goal "put it in plana" must be split, because **plana today is
a pure protocol/type crate** — only `serde` / `ts-rs` / `schemars`
dependencies, `lib.rs:5` mandates "every type is defined in entelecheia
and consumed by shittim-chest", and it `exclude`s all non-protocol
artefacts for crates.io publishing. Injecting runtime logic (tokio,
`sd_listen_fds`, signal handling) would break that lightweight,
publishable identity.
Split:
- **`plana::lifecycle` (protocol contract, lives in plana).** Only
  JSON-RPC methods and types: `DrainState`, `ReadyStatus`,
  `Lifecycle.Drain`, `Lifecycle.Status`, `Worker.Status`, etc. Satisfies
  plana's "paired on both sides" rule.
- **`malkuth` (standalone crate, runtime).** Depends on `tokio`
  (pure Rust — no `libsystemd`). Protocol types are self-contained
  within `malkuth`. Feature-gated:
  - `replica` — Subsystem A coordination + orchestration.
  - `leader-follower` — Subsystem B lease election + failover.
  - `socket-activation` — systemd fd acquisition.
  - `file-lock` / `pg-lock` / `lease` — `CoordinationLock` backends.
The three projects depend on `malkuth` and enable the features
they need (see §8 matrix). Putting everything in plana would force it to
become "protocol + optional runtime" and destroy its purity — not
recommended.
## 5. Core Abstractions
### 5.1 `Worker` — a supervised child-process resource
A `Worker` is one independently-killable process that holds exactly one
resource (a PLC connection, a serial port, a local listening port, a
sidecar like cosmos / pglite-proxy). The process is the **failure
isolation boundary**: a bug in the Modbus stack cannot poison the
S7comm worker.
Lifecycle FSM (lifted from `evernight/src/model_server.rs:128-139`):
```
        start                      health ok
 Starting ──────► Running ─────────────────► Running
     │              │  ▲
     │              │  │ health ok (self-heal)
     │              ▼  │
     └──────► Failed ◄┘        crash / unhealthy
                  │
                  │ restart policy = permanent
                  └────────► Starting (rate-limited)
```
### 5.2 `Supervisor` — owns the worker pool
- **Restart policy** (OTP vocabulary): `permanent` (always restart —
  default for resource workers), `transient` (restart only on abnormal
  exit), `temporary` (never restart).
- **Sliding-window rate limiting** (lifted from entelecheia
  `health_daemon` `max_restart_attempts` + `cooldown`): if a worker
  restarts more than N times in window W, it enters `cooldown` to
  prevent crash storms; further restarts are deferred.
### 5.3 `Lifecycle` — uniform signal semantics (Layer 1)
Adopt the nginx/Go convention:
| Signal | Semantics | Behaviour |
|---|---|---|
| `SIGINT` (ctrl_c) | equivalent to SIGTERM (dev-friendly) | enter drain |
| `SIGTERM` | **graceful shutdown** | drain: clear ready bit → stop accepting → drain in-flight → exit |
| `SIGHUP` | **hot config reload** | do not exit; re-read config |
| `SIGQUIT` | **immediate exit** (emergency only) | skip drain, fast exit |
**Drain sequence** (one implementation; each project injects its own
"drain closure"):
1. Set `/readyz` `draining = true` (LB / orchestrator sees this and
   stops sending new traffic).
2. Stop `accept` on new connections (under socket activation: stop
   accepting from the inherited fd).
3. Send close frames to live WebSockets; wait for in-flight requests
   with a timeout `DRAIN_TIMEOUT` (default 30s, configurable).
4. Drain background tasks (copy entelecheia `TaskManager.stop_all` +
   `wait_all`).
5. Disconnect upstream pools cleanly (copy shittim-chest
   `upstream_pool` clean disconnect).
6. Release locks, clean temp files → exit 0.
Implementation note: axum's `axum::serve(listener,
app).with_graceful_shutdown(...)` already supports drain; **the key
missing piece is wiring `SIGTERM` in** (today only `ctrl_c` is wired).
References: `entelecheia/.../shutdown.rs:17`,
`shittim-chest/.../api.rs:465`, `evernight/src/api/mod.rs:109`.
### 5.4 Health endpoints (uniform)
Split probes (today the three projects are inconsistent):
| Endpoint | Semantics | Decision |
|---|---|---|
| `/healthz` (liveness) | process alive | 200 if the process can answer (simple restart criterion) |
| `/readyz` (readiness) | **can serve**, carries drain bit | 200 if not draining AND dependencies up (DB ping / scepter socket / first station poll); 503 while draining |
The `/readyz` `draining` bit is the central rolling-update signal: the
orchestrator routes new requests only to instances whose `/readyz` is
200. shittim-chest's existing `GET /api/health` (`routes.rs:27`) is
upgraded to `/readyz` with a drain bit.
### 5.5 `acquire_listener` — Layer 3 zero-downtime handoff
`malkuth` exposes `acquire_listener(addr) -> TcpListener`:
1. Try `sd_listen_fds()` (validate `LISTEN_PID`) — systemd is holding
   the fd.
2. Fallback to plain `TcpListener::bind(addr)` (dev, no systemd).
axum `serve(listener, ...)` already accepts a pre-bound listener, so the
plumbing exists; only the fd *source* is missing today. Three deploy
adapters:
| Deploy | Approach | Applies to |
|---|---|---|
| **bare systemd** | `xxx.socket` + `xxx@.service` template instances | scepter, evernight-gateway, malkuth itself |
| **docker** (shittim-chest prod) | host systemd socket activation, pass the bound socket/fd into the container (`LISTEN_FDS` + `SocketUser`); or a lightweight in-container master holding the fd | shittim-chest prod |
| **dev** | fallback plain `bind` + brief overlap (accept a few hundred ms of dropped connections), no systemd | all three dev |
Rolling update under socket activation:
```
[upgrade trigger] → start new instance (templated service@new, inherits/re-acquires fd)
                   → poll new instance /readyz until 200
                   → SIGTERM the old instance (= drain)
                   → old instance drains and self-exits
                   → systemd holds the fd throughout → zero dropped connections
```
### 5.6 `CoordinationLock` — Layer 2 trait with backends
During the rolling-update window, old and new instances may
concurrently read/write shared resources. DB transactions are naturally
safe; files (evernight JSONL, configs) need "notify-before-write + lock".
```rust
pub trait CoordinationLock: Send + Sync {
    async fn acquire(&self, key: &str, lease: Duration) -> Result<Box<dyn LockGuard>, LockError>;
}
// Backends:
//   FileLock  — flock/fcntl,       for evernight (JSONL / config)
//   PgLock    — pg_advisory_lock,  for entelecheia / shittim-chest
//   LeaseLock — file lock + lease (auto-expiry on crash)
```
**Instance registry** (used only during the upgrade window; a single
record in steady state): a small shared table/file recording
`{instance_id, role: Active | Draining, started_at, generation}`. The
new instance writes an `Active` row on start; on upgrade the old one is
marked `Draining`. This replaces the Raft quorum that was explicitly
out-of-scope — because steady state is single-instance, the registry
only coordinates drain, with no strong-consistency need (a file or DB
row suffices).
## 6. Subsystem A — Replica (load-balancing ⊃ rolling update)
**Shape.** N identical peer instances running in parallel behind a
fronting LB, state in shared Postgres. **active-active**, peers are
equal, no leader.
| Concern | Approach |
|---|---|
| Request routing | fronting LB (caddy / built-in `SO_REUSEPORT` round-robin); removal by `/readyz` |
| Shared state R/W | DB transactions + `pg_advisory_lock` for concurrent-write coordination (naturally safe) |
| WebSocket / long connections | **sticky session** (LB pins by cookie/instance id) or **session migration** (client reconnects to any replica after disconnect; state recovered from DB — shittim-chest `upstream_pool` is already a reconnect template) |
| Session affinity | entelecheia and shittim-chest both externalize state to Postgres and recover on boot → **naturally replica-friendly**, their key advantage over evernight |
| Rolling update | replica-scale sub-operation: add new replica (new version) → ready → drain & remove old replica → repeat. K8s `maxSurge/maxUnavailable` in miniature |
**Why A is comparatively easy.** Because entelecheia and shittim-chest
externalize state to Postgres (recover on boot, confirmed by audit),
replicas need no state replication between them — a fronting LB plus DB
transactions suffices. The only hard part is WebSocket stickiness /
migration.
## 7. Subsystem B — Leader/Follower (edge active-passive HA)
**Shape.** Two evernight processes on the same device/gateway, one
leader one follower; the upstream `evernight-server` sees **one
`node_id`** (one device). **active-passive**, instances are not equal,
the leader exclusively owns physical I/O.
### 7.1 The "trick" that simplifies B: supervisor tree + let-it-crash
Rather than fault-tolerating every resource, **only the supervisor is
made leader/follower**; the resources are independent worker processes
that are simply restarted on crash. Concretely (per agreed decision):
```
supervisor  (leader / follower HA)        ← only this layer does lease election + failover
   ├─ worker: PLC-A (Modbus)              ← supervised restart, single instance
   ├─ worker: PLC-B (S7comm)              ← supervised restart, single instance
   ├─ worker: serial / CAN                ← supervised restart, single instance
   └─ worker: local port listener         ← supervised restart, single instance
```
This is the OTP supervision-tree / "let it crash" model (also systemd
`Restart=always`, K8s Pod). Benefits:
- **Separation of concerns.** Workers only "hold one resource and do
  work"; they carry no fault-tolerance, election, or state-sync logic —
  they stay maximally simple.
- **Fault tolerance concentrated.** Only the supervisor does HA; the
  complexity collapses to one place.
- **Failure isolation.** A crash in one resource (e.g. one PLC
  protocol) cannot affect others (separate processes).
- **Protocol-fit.** evernight speaks many industrial protocols
  (Modbus/S7/CAN/serial); mapping each to a worker gives maximal
  isolation value.
### 7.2 Worker lifecycle on failover — child-process model (agreed starting point)
Workers are **child processes** of the supervisor (`kill_on_drop`).
Leader death → workers are orphaned/killed → the promoted follower
**re-spawns all workers** (each PLC reconnects).
- Simplest model; matches "the server is responsible" intent.
- Cost: on supervisor failover, all resources briefly drop and reconnect.
- Advanced (deferred): workers as independent daemons under a lower
  init, supervisor only directs them via IPC; the new supervisor
  `attach`es surviving workers. Lower interruption, but workers must
  implement "re-bind to current supervisor" — more complex. Documented
  as a future option.
### 7.3 Leader election + fencing
- **Lease election.** The leader holds a file lock + lease (with TTL),
  renews every heartbeat; the follower polls; on leader heartbeat
  timeout the follower seizes the lease and promotes itself.
- **Exclusive physical resource.** PLC/serial/CAN connections can only
  be held by the leader (two processes polling the same PLC conflict) →
  leader polls, follower stands by. This is the fundamental reason B is
  active-passive, not active-active.
- **State sync.** **Cold standby** as the starting point (follower does
  not replicate; on promotion it recovers from JSONL on disk). Hot
  standby (follower tails the leader's JSONL) is an advanced option.
- **Split-brain fencing.** Lease TTL + fencing: the follower may seize
  only after the lease truly expires; after seizing, the old leader is
  physically prevented from further writes (physical-I/O exclusivity is
  a natural fence).
- **Single device identity.** Leader & follower share one `node_id`;
  only the current leader emits `device.register`.
Classic analogues: keepalived/VRRP, DRBD+Pacemaker, MySQL
primary/replica, Redis Sentinel — as an in-machine, process-level
simplification. The theory (lease election + fencing) is well-founded.
## 8. Per-project Adoption Matrix
| Project | supervisor role | workers | strategy |
|---|---|---|---|
| entelecheia (scepter) | one of the replicas | cosmos sidecar, agent containers | **A Replica** |
| shittim-chest (chest) | one of the replicas | pglite-proxy (mock), channel intake | **A Replica** |
| evernight device (`sensor-poll`) | **leader / follower** | one worker per protocol (Modbus/S7/CAN/serial) | **B Leader/Follower** |
| evernight-server (central) | one of the replicas | model_server containers | **A Replica** |
Feature selection per project (`malkuth`):
- entelecheia / shittim-chest / evernight-server: `replica` +
  `socket-activation` + `pg-lock`; worker abstraction for their sidecars.
- evernight device: `leader-follower` + `socket-activation` (advanced fd
  takeover) + `file-lock` / `lease`; worker abstraction for per-protocol
  processes.
## 9. Rollout Phases
1. **Phase A — Layer 1 across all three projects.** Signal semantics +
   `/healthz` / `/readyz` + drain. Lowest risk, highest immediate payoff
   (fixes SIGTERM hard-kill first).
2. **Phase B — `plana::lifecycle` protocol + `malkuth`
   skeleton.** Trait definitions, `acquire_listener`,
   `CoordinationLock` trait + `FileLock` / `PgLock` backends, `Worker` +
   `Supervisor` primitives.
3. **Phase C — Layer 3.** Socket-activation units for the three projects
   + the docker adapter + dev fallback.
4. **Phase D — Layer 4.** Manifest-queue orchestrator (copy
   `health_daemon`) + old/new coexistence drain loop, including the dev
   "compile new server" workflow; plus leader/follower failover for B.
## 10. Risks & Boundaries
- **shittim-chest docker + socket activation** is the most uncertain
  piece; the fd-into-container handoff needs a prototype spike. If
  infeasible, fall back to "external caddy + `/readyz` removal + brief
  overlap".
- **evernight in-memory state** (`DeviceRegistry`, sessions) is lost on
  drain; decide whether to persist or migrate (worst case: the new
  instance rebuilds, long sessions drop).
- **WebSocket drain ≠ migration.** In-flight WS still drops when the old
  instance exits; "seamless" requires the client to reconnect to a new
  instance (shittim-chest client already has `upstream_pool` reconnect
  logic, reusable).
- **Process-level vs thread-level fault tolerance.** Leader/follower
  (Subsystem B) solves *process/instance-level* fault tolerance, not
  thread-level. An in-task tokio panic is the supervisor's job (task
  restart, not process failover). Do not use B to absorb thread crashes
  — that is far too heavy.
- **Explicitly out of scope.** Raft quorum, consistent-hash sharding,
  cross-datacenter HA — excluded by the "rolling-update + edge HA"
  scope. Single-instance steady state means the registry only
  coordinates drain.
## 11. Open Questions (deferred)
- Worker-on-failover: child-process model chosen for the starting point;
  the "independent daemon + attach" model is documented as advanced.
- Cold vs hot standby for B: cold chosen for the starting point
  (recover from JSONL on promotion); hot (follower tails leader log)
  deferred.
- Whether to also wrap entelecheia's `cosmos` sidecar and
  shittim-chest's `pglite-proxy` under the unified `Worker` abstraction:
  agreed yes (unify into one worker abstraction across all three
  projects).
---
*Translation: `docs/zhs/design/supervision-and-rolling-update.md`
is the Simplified Chinese counterpart. Other languages (zht/ja/ko/fr/es/ru/ar)
have been translated as well (see `docs/<lang>/design/`).*

---

## Appendix: 0.1 implementation consolidation (SySL)

The 0.1 implementation made two consolidations relative to the original design
above, both decisions taken for simplicity without changing the architecture's
trait boundaries:

1. **Tokio-only async layer.** The library is built directly on `tokio`
   (`tokio::net`, `tokio::process`, `tokio::signal`, `tokio-tungstenite`).
   Runtime-pluggability (async-std / smol) was dropped: `interprocess`'s async
   support is tokio-only, `pg_advisory_lock` is tokio-postgres, and tokio is the
   de-facto Rust executor. The contract layer (now folded into `malkuth`) remains
   runtime-light (no hard runtime dependency).

2. **CoordinationLock backends all implemented.** `file-lock` (POSIX `flock`),
   `lease` (file lease + TTL auto-expiry, the leader-election primitive), and
   `pg-lock` (`pg_advisory_lock`) are all implemented, as is the `leader-follower`
   `LeaseLeaderElector` over the lease backend.

The transport abstraction (`Transport` / `WireConn` over `tcp` / `ws` / `ipc`)
and the pluggable hook traits (`ExitSource`, `ProbeSink`, `Heartbeat`,
`DrainHook`) are exactly as designed in §3–§5; only the concrete async
primitives were consolidated onto tokio.
