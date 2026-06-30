# Quick Start

## Add malkuth to your project

```toml
[dependencies]
malkuth = { git = "https://github.com/celestia-island/malkuth.git", branch = "dev" }
# Optional features:
#   socket-activation  — inherit a listener fd from systemd
#   file-lock          — POSIX flock coordination-lock backend
#   lease              — lease-based file lock with TTL auto-expiry (staged)
#   pg-lock            — PostgreSQL advisory lock (staged)
#   replica            — InstanceRegistry trait (load-balancing)
#   leader-follower    — LeaderElector trait (active-passive HA)
```

## Minimal server with graceful shutdown and probes

```rust
use malkuth::{acquire_listener, probe_router, ProbeState, DrainController};

#[tokio::main]
async fn main() -> std::io::Result<()> {
    // 1. Acquire a listener — prefers systemd socket activation,
    //    falls back to binding the given address.
    let listener = acquire_listener("0.0.0.0:8080").await?;

    // 2. Create probe state and install the drain controller.
    let probe = ProbeState::new(env!("CARGO_PKG_VERSION"));
    let ctrl = DrainController::install();

    // 3. Build your router, merging the probe routes.
    let app = axum::Router::new()
        .route("/", axum::routing::get(|| async { "hello" }))
        .merge(probe_router(probe))
        .with_state(());

    // 4. Serve with graceful shutdown: SIGINT/SIGTERM trigger drain,
    //    SIGQUIT forces immediate exit, SIGHUP reloads (keeps serving).
    axum::serve(listener, app)
        .with_graceful_shutdown(async {
            ctrl.wait_for_drain().await;
        })
        .await?;
    Ok(())
}
```

## What you get

| Endpoint | Purpose |
| --- | --- |
| `GET /healthz` | Liveness — "the process is alive" (pid, uptime, version) |
| `GET /readyz` | Readiness — "can serve" (returns 503 while draining) |

| Signal | Behaviour |
| --- | --- |
| `SIGINT` / `SIGTERM` | Graceful drain (finish in-flight, then exit) |
| `SIGHUP` | Hot reload (does **not** exit — server keeps serving) |
| `SIGQUIT` | Immediate exit (emergency only) |

## Feature flags

| Feature | What it enables |
| --- | --- |
| `socket-activation` | Inherit a listener fd from systemd (zero-downtime restart) |
| `file-lock` | POSIX `flock`-based `CoordinationLock` backend (Unix only) |
| `lease` | Lease-based file lock with TTL auto-expiry on crash (staged) |
| `pg-lock` | PostgreSQL advisory lock backend (staged) |
| `replica` | `InstanceRegistry` trait for load-balanced replicas |
| `leader-follower` | `LeaderElector` trait for active-passive HA |

All features are opt-in; the default build has no unsafe code and depends only on tokio + axum.
