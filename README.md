<!-- markdownlint-disable MD033 MD041 MD036 -->
<div align="center">

# Plana

**Generic Rust toolkit for service supervision**

[![License](https://img.shields.io/badge/license-BSL--1.1-blue.svg)](LICENSE)
[![Rust](https://img.shields.io/badge/rust-1.85%2B-orange.svg)](https://www.rust-lang.org/)
[![GitHub](https://img.shields.io/badge/github-celestia--island%2Fplana-blue.svg)](https://github.com/celestia-island/plana)

</div>
<!-- markdownlint-enable MD033 MD041 MD036 -->

> **Version 0.1.0** — Originally factored out of the celestia-island platform
> ([entelecheia](https://github.com/celestia-island/entelecheia),
> [shittim-chest](https://github.com/celestia-island/shittim-chest),
> [evernight](https://github.com/celestia-island/evernight)).

Plana provides the building blocks for running long-lived Rust services that
need to **survive restarts**, **roll out updates without dropping
connections**, and **coordinate across instances**:

- **Lifecycle** — uniform signal semantics (`SIGTERM` / `SIGINT` = drain,
  `SIGHUP` = reload, `SIGQUIT` = immediate) via [`DrainController`].
- **Probes** — split `/healthz` (liveness) + `/readyz` (readiness, with a
  drain bit) so load balancers and orchestrators can route and retire nodes.
- **Workers** — supervised child-process resources, each a failure-isolation
  boundary, with OTP-style restart policy (`permanent` / `transient` /
  `temporary`) and sliding-window rate limiting.
- **Listener handoff** — systemd socket activation (pure-Rust, no
  `libsystemd`) with a plain-bind fallback, for zero-downtime rolling updates.
- **Coordination locks** — a pluggable [`CoordinationLock`] trait
  (`file-lock` / `pg-lock` / `lease`) for coordinating concurrent writes or
  leader election.

[`DrainController`]: https://docs.rs/plana/latest/plana/struct.DrainController.html
[`CoordinationLock`]: https://docs.rs/plana/latest/plana/trait.CoordinationLock.html

## Quick start

```rust
use plana::{acquire_listener, probe_router, ProbeState, DrainController};

#[tokio::main]
async fn main() -> std::io::Result<()> {
    // Layer 3: socket activation, falls back to a plain bind.
    let listener = acquire_listener("0.0.0.0:8080").await?;

    // Layer 1: probes + signal-aware drain.
    let probe = ProbeState::new(env!("CARGO_PKG_VERSION"));
    let ctrl = DrainController::install();

    let app = axum::Router::new()
        .merge(probe_router(probe)) // GET /healthz, GET /readyz
        .with_state(());

    axum::serve(listener, app)
        .with_graceful_shutdown(async {
            // Resolves on SIGINT / SIGTERM (drain) or SIGQUIT (immediate),
            // but NOT on SIGHUP (reload — server keeps serving).
            ctrl.wait_for_drain().await;
        })
        .await?;
    Ok(())
}
```

## Feature flags

| Feature | Enables |
| --- | --- |
| `socket-activation` | inherit systemd listener fd (`acquire_listener`) |
| `file-lock` | POSIX `flock` `CoordinationLock` backend |
| `lease` | lease-based file lock with TTL auto-expiry |
| `pg-lock` | PostgreSQL `pg_advisory_lock` backend (staged) |
| `replica` | Subsystem A `InstanceRegistry` trait (load-balancing / rolling update) |
| `leader-follower` | Subsystem B `LeaderElector` trait (active-passive HA) |

## Status

Early. Layer 1 (lifecycle + probes), supervised workers, listener handoff
and the coordination-lock trait with the `file-lock` backend are
implemented. The `replica` / `leader-follower` strategy backends are trait
contracts with full implementations staged. See the design doc at
[`arona` docs/\<lang\>/design/platform/supervision-and-rolling-update.md](https://github.com/celestia-island/arona/tree/dev/docs/en/design/platform)
for the full architecture.

## License

Business Source License 1.1 (BSL-1.1); automatically converts to your choice
of Apache-2.0 or MIT on 2030-01-01. See [LICENSE](LICENSE).
