# Health Probes

## The `ProbeSink` trait

Malkuth separates **probe state** from **how it is exposed**. The
[`ProbeSink`](../design/supervision-and-rolling-update.md) trait defines two queries:

```rust
#[async_trait]
pub trait ProbeSink: Send + Sync {
    async fn ready(&self) -> ReadyStatus;
    async fn health(&self) -> HealthStatus;
}
```

Any type that implements `ProbeSink` can be queried over JSON-RPC or HTTP.

## `ProbeState` — the built-in implementation

`ProbeState` holds version info, a drain-state flag, a generation counter, and
a list of dependency checks:

```rust
use malkuth::{ProbeState, DrainState};

let probe = ProbeState::new(env!("CARGO_PKG_VERSION"));

// Register a dependency that affects readiness.
// The closure is synchronous — keep it cheap (read an atomic, ping a cached conn).
probe.add_dependency("database", || { /* return true if healthy */ true });

// Flip the drain bit during shutdown:
probe.set_drain_state(DrainState::Draining);

// Record the deployment generation (visible in the status response):
probe.set_generation(Some(2));
```

## JSON-RPC exposure (primary)

`Router::lifecycle(ctrl, Some(probe))` registers the standard methods,
querying the `ProbeSink` on each call:

```rust
use std::sync::Arc;
use malkuth::{ProbeState, Router, Supervised};
use malkuth::transport::TcpTransport;
use malkuth::Transport;

let supervised = Supervised::new().signals();
let ctrl = supervised.drain_controller();
let probe = Arc::new(ProbeState::new("0.1.0"));

let handler = Arc::new(
    Router::new()
        .lifecycle(ctrl, Some(probe.clone()))
        .route("ping", |_| Box::pin(async { Ok(serde_json::json!("pong")) })),
);

supervised.serve_rpc(&TcpTransport, "tcp://0.0.0.0:8080", handler).await?;
```

### `Lifecycle.Health` → `HealthStatus`

```json
// Request: { "jsonrpc": "2.0", "id": 1, "method": "Lifecycle.Health", "params": {} }
// Response:
{ "alive": true, "pid": 12345, "uptime_secs": 360, "version": "0.1.0" }
```

### `Lifecycle.Status` → `ReadyStatus`

```json
// Request: { "jsonrpc": "2.0", "id": 2, "method": "Lifecycle.Status", "params": {} }
// Response:
{
  "ready": true,
  "draining": false,
  "dependencies": [{ "name": "database", "ok": true }],
  "generation": 2
}
```

When `draining` is `true` or any dependency is `ok: false`, `ready` is `false`.

## HTTP exposure (optional, feature `probes`)

For Kubernetes-style HTTP probes or external load balancers that expect HTTP,
enable the `probes` feature to get axum routes:

```rust
use malkuth::{ProbeState, probe_router};

let probe = ProbeState::new(env!("CARGO_PKG_VERSION"));
probe.add_dependency("database", || { true });

let app = axum::Router::new()
    .merge(probe_router(probe));   // GET /healthz + GET /readyz
```

| Endpoint | Returns | HTTP status |
| --- | --- | --- |
| `GET /healthz` | `HealthStatus` | Always 200 |
| `GET /readyz` | `ReadyStatus` | 200 if ready, 503 if draining / dep down |

The response shapes are identical to the JSON-RPC methods — `ProbeState`
implements `ProbeSink`, so both paths query the same underlying state.

## Wiring drain into probes

During graceful shutdown, set the drain state so `Lifecycle.Status` (and
`/readyz`) reflect it:

```rust
use malkuth::{DrainController, DrainState, ShutdownKind};

let ctrl = DrainController::new();
let probe = ProbeState::new("0.1.0");

tokio::spawn({
    let probe = probe.clone();
    let ctrl = ctrl.clone();
    async move {
        ctrl.wait_for_drain().await;
        probe.set_drain_state(DrainState::Draining);
    }
});
```

Now the orchestrator sees readiness flip to `false` **before** the process
exits — the core of zero-downtime rolling updates.
