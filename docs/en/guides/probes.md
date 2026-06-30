# Health Probes

## Split probes: `/healthz` vs `/readyz`

Malkuth follows the Kubernetes convention of **two separate probe endpoints**:

- **`GET /healthz`** — *Liveness*: "Is the process alive?" If this fails, the
  orchestrator **restarts** the instance.
- **`GET /readyz`** — *Readiness*: "Can this instance serve traffic right now?"
  If this fails, the orchestrator **stops routing traffic** but does not restart.

The distinction matters during rolling updates: an instance that is draining
is *alive* (healthz = 200) but *not ready* (readyz = 503).

## Setup

```rust
use malkuth::{ProbeState, probe_router};

let probe = ProbeState::new(env!("CARGO_PKG_VERSION"));

// Register dependencies that affect readiness:
probe.add_dependency("database", || {
    // Return true if the DB connection is healthy.
    // This is a sync closure — keep it cheap (read an atomic, ping a cached conn).
    true
}).await;

// Merge the probe routes into your app:
let app = axum::Router::new()
    .merge(probe_router(probe));
```

## Response shapes

### `/healthz` (always 200 if the process can answer)

```json
{
  "alive": true,
  "pid": 12345,
  "uptime_secs": 3600,
  "version": "0.1.0"
}
```

### `/readyz` (503 when not ready)

```json
{
  "ready": true,
  "draining": false,
  "dependencies": [
    { "name": "database", "ok": true }
  ],
  "generation": 2
}
```

When draining or a dependency is unhealthy, `ready` is `false` and the HTTP
status is `503 Service Unavailable`.

## Wiring the drain bit

During graceful shutdown, set the draining flag so `/readyz` starts returning 503:

```rust
use malkuth::DrainController;

let probe = ProbeState::new(env!("CARGO_PKG_VERSION"));
let ctrl = DrainController::install();

// In your shutdown sequence:
tokio::spawn({
    let probe = probe.clone();
    let ctrl = ctrl.clone();
    async move {
        // Wait until drain begins, then flip the bit.
        ctrl.wait_for_drain().await;
        probe.set_draining(true).await;
    }
});
```

Now the load balancer sees `/readyz` go 503 and stops sending new traffic
**before** the process exits — the core of zero-downtime rolling updates.

## Deployment generation

Track which deployment generation this instance belongs to:

```rust
probe.set_generation(Some(2)).await; // generation 2 of a rolling update
```

This is included in the `/readyz` response for observability and orchestration.
