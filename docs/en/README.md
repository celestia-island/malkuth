<!-- markdownlint-disable MD033 MD041 MD036 -->
<div align="center">

<img src="../res/logo.webp" alt="Plana logo" width="200"/>

# Plana

**Infrastructure for long-running programs to upgrade themselves and balance load**

[![License](https://img.shields.io/badge/license-BSL--1.1-blue.svg)](../LICENSE)
[![Rust](https://img.shields.io/badge/rust-1.85%2B-orange.svg)](https://www.rust-lang.org/)
[![GitHub](https://img.shields.io/badge/github-celestia--island%2Fplana-blue.svg)](https://github.com/celestia-island/plana)

</div>
<!-- markdownlint-enable MD033 MD041 MD036 -->

**[English](../../README.md)** &bull; **[简体中文](../zhs/README.md)** &bull;
**[繁體中文](../zht/README.md)** &bull; **[日本語](../ja/README.md)** &bull;
**[한국어](../ko/README.md)** &bull; **[Français](../fr/README.md)** &bull;
**[Español](../es/README.md)** &bull; **[Русский](../ru/README.md)**

> **Version 0.1.0** — Early development. Independent and self-contained;
> depends only on tokio + axum.

Plana helps automated, long-running programs — daemons, agents, servers — do
two hard things safely:

- **Self-upgrade** — roll out a new version (or a freshly compiled build)
  without dropping in-flight work or connections: zero-downtime rolling updates.
- **Load balancing** — run multiple instances that share work and coordinate
  state, where one can retire gracefully while another takes over.

## Building blocks

- **Lifecycle** — uniform signal semantics (`SIGTERM` / `SIGINT` = drain,
  `SIGHUP` = reload, `SIGQUIT` = immediate) via `DrainController`.
- **Probes** — split `/healthz` (liveness) + `/readyz` (readiness, with a drain
  bit) so load balancers and orchestrators can route and retire nodes.
- **Workers** — supervised child-process resources, each a failure-isolation
  boundary, with OTP-style restart policy and sliding-window rate limiting.
- **Listener handoff** — socket-activation listener inheritance with a
  plain-bind fallback, for zero-downtime restarts.
- **Coordination locks** — a pluggable `CoordinationLock` trait
  (`file-lock` / `pg-lock` / `lease`) for coordinating concurrent writes or
  leader election.

## Quick Start

```toml
[dependencies]
plana = { git = "https://github.com/celestia-island/plana.git", branch = "dev" }
# features: socket-activation, file-lock, lease, pg-lock, replica, leader-follower
```

```rust
use plana::{acquire_listener, probe_router, ProbeState, DrainController};

#[tokio::main]
async fn main() -> std::io::Result<()> {
    // Listener handoff: socket activation, falls back to a plain bind.
    let listener = acquire_listener("0.0.0.0:8080").await?;

    // Probes + signal-aware drain.
    let probe = ProbeState::new(env!("CARGO_PKG_VERSION"));
    let ctrl = DrainController::install();

    let app = axum::Router::new()
        .merge(probe_router(probe)) // GET /healthz, GET /readyz
        .with_state(());

    axum::serve(listener, app)
        .with_graceful_shutdown(async {
            // Resolves on SIGINT / SIGTERM (drain) or SIGQUIT (immediate),
            // but NOT on SIGHUP (reload — the server keeps serving).
            ctrl.wait_for_drain().await;
        })
        .await?;
    Ok(())
}
```

## Feature flags

| Feature | Enables |
| --- | --- |
| `socket-activation` | inherit a listener fd (socket activation) |
| `file-lock` | POSIX `flock` `CoordinationLock` backend |
| `lease` | lease-based file lock with TTL auto-expiry |
| `pg-lock` | PostgreSQL `pg_advisory_lock` backend (staged) |
| `replica` | `InstanceRegistry` trait (load-balancing / rolling update) |
| `leader-follower` | `LeaderElector` trait (active-passive HA) |

## Status

Lifecycle + probes, supervised workers, listener handoff and the
coordination-lock trait with the `file-lock` backend are implemented. The
`replica` / `leader-follower` strategy backends are trait contracts with full
implementations staged.

## License

Business Source License 1.1 (BSL-1.1); automatically converts to your choice
of Apache-2.0 or MIT on 2030-01-01. See [LICENSE](../LICENSE).
