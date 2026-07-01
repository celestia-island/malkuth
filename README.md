# Malkuth
<!-- markdownlint-disable MD033 MD041 MD036 -->
<div align="center">

<img src="docs/logo.webp" alt="Malkuth" width="200"/>

**Composable service-supervision toolkit for Rust — JSON-RPC over pluggable transports, supervised workers, coordination locks & leader election, plus a watchdog CLI.**

[![License](https://img.shields.io/badge/license-SySL%201.0-blue)](LICENSE)
[![Rust](https://img.shields.io/badge/rust-1.85%2B-orange.svg)](https://www.rust-lang.org/)
[![GitHub](https://img.shields.io/badge/github-celestia--island%2Fmalkuth-blue.svg)](https://github.com/celestia-island/malkuth)

</div>
<!-- markdownlint-enable MD033 MD041 MD036 -->

**[English](docs/en/README.md)** &bull; **[简体中文](docs/zhs/README.md)** &bull;
**[繁體中文](docs/zht/README.md)** &bull; **[日本語](docs/ja/README.md)** &bull;
**[한국어](docs/ko/README.md)** &bull; **[Français](docs/fr/README.md)** &bull;
**[Español](docs/es/README.md)** &bull; **[Русский](docs/ru/README.md)** &bull;
**[العربية](docs/ar/README.md)**

> **Version 0.1.0** — Workspace of three crates, **tokio-based**. The CLI wraps
> *any* program (even one that does not use the library) with a pod pool and a
> sticky reverse proxy.

Malkuth helps automated, long-running programs do four hard things:

1. **Pluggable transport** — JSON-RPC over local TCP loopback, remote
   **WebSocket**, or local **IPC** (Unix sockets / named pipes via
   [`interprocess`](https://crates.io/crates/interprocess)). One `Transport`
   trait, dispatched by URL scheme.
2. **Tokio-based, framework-light** — built on `tokio`; the JSON-RPC path needs
   no HTTP framework (axum is optional, for HTTP probes only).
3. **Optional, hookable facilities** — exit source, probes, heartbeat and drain
   hooks are *traits*. Use the defaults (OS-signal exit, axum probes, supervised
   workers) or supply your own (e.g. trigger drain from an in-band "stop" command
   your server receives). A batteries-included `Supervised` orchestrator wires
   them together.
4. **A watchdog CLI** — `malkuth -- <cmd>` wraps a program with file watching, a
   pod pool, and an L4 sticky reverse proxy.

## Workspace layout

| Crate | What it is |
| --- | --- |
| **`malkuth-core`** | Runtime-light **contracts**: wire types + traits (`Transport`, `WireConn`, `CoordinationLock`, `ExitSource`, `ProbeSink`, `Heartbeat`, `DrainHook`, `LeaderElector`, `InstanceRegistry`). Depends only on `serde` + `event-listener` + `async-trait`. |
| **`malkuth`** | Tokio **implementations**: JSON-RPC codec, server/client, transports (tcp/ws/ipc), supervised workers, probes, signals, coordination-lock backends (file/lease/pg), lease-based leader election. |
| **`malkuth-cli`** | The `malkuth` binary — pod pool + file watcher + sticky reverse proxy. |

## The CLI (wraps anything)

```
malkuth [--watch PATH]... [--proxy PUBLIC:LO-HI] [--pod-count N] -- <cmd> [args...]
```

Run 5 parallel copies of your server (each listening on the `PORT` env var →
they self-assign 3001–3005), fronted by a sticky proxy on 3000:

```bash
malkuth --watch ./src --watch ./res \
        --proxy 3000:3000-3999 --pod-count 5 \
        -- cargo run
```

The proxy routes each **client IP** to a fixed backend via consistent hashing, so
a client keeps hitting the same pod until that pod restarts or scales down — the
basis for gray release / rolling restart. On a file change it drains and
restarts one pod at a time.

## The library (embed in your own service)

```toml
[dependencies]
malkuth = { git = "https://github.com/celestia-island/malkuth.git", branch = "dev" }
# features: tcp (default) | ws | ipc | signals (default) | worker | axum-probe |
#           file-lock | lease | pg-lock | replica | leader-follower
```

```rust
use std::sync::Arc;
use malkuth::{Client, Router, Server, Supervised};
use malkuth::transport::TcpTransport;
use malkuth_core::Transport;
use serde_json::json;

#[tokio::main]
async fn main() -> std::io::Result<()> {
    // Bind once; build a router with the standard lifecycle RPC + your methods.
    let lis = TcpTransport.listen("tcp://127.0.0.1:0").await?;
    let supervised = Supervised::new().signals();           // OS-signal exit source
    let ctrl = supervised.drain_controller();
    let handler = Arc::new(
        Router::new()
            .lifecycle(ctrl, None)                          // Lifecycle.Drain/Status/...
            .route("ping", |_| Box::pin(async { Ok(json!("pong")) })),
    );
    // Race the server against the exit source, then run drain hooks.
    supervised.serve_rpc_listener(lis, handler).await
}
```

Need drain triggered by your own logic instead of signals? Implement
`malkuth_core::ExitSource` and pass it via `.exit(...)`. Want Postgres-backed
coordination? `PgLock::new(Arc::new(client))` implements `CoordinationLock`.

## Feature flags (`malkuth`)

| Feature | Enables |
| --- | --- |
| `tcp` *(default)* | JSON-RPC over local/remote TCP (`tokio::net`) |
| `ws` | JSON-RPC over WebSocket (`tokio-tungstenite`) |
| `ipc` | JSON-RPC over local IPC (`interprocess`) |
| `signals` *(default)* | Default OS-signal `ExitSource` (`tokio::signal`) |
| `worker` | Supervised child-process workers (`tokio::process`) |
| `axum-probe` | axum `/healthz` + `/readyz` router |
| `file-lock` | POSIX `flock` `CoordinationLock` backend (unix) |
| `lease` | File-lease `CoordinationLock` with TTL auto-expiry (crash-safe) |
| `pg-lock` | PostgreSQL `pg_advisory_lock` backend (`tokio-postgres`) |
| `replica` | In-memory `InstanceRegistry` |
| `leader-follower` | `LeaseLeaderElector` (Subsystem B, over the lease backend) |

## Status

Layers 1–3 (lifecycle/drain, probes, listener handoff) and the JSON-RPC core
(codec + server/client + tcp/ws/ipc transports) are implemented and tested
end-to-end. The CLI pod pool + sticky proxy is working (e2e-verified). All three
`CoordinationLock` backends (`file-lock`, `lease`, `pg-lock`) and the
`leader-follower` `LeaseLeaderElector` are implemented. See
[docs/design/](docs/en/design/) for the design.

## License

[Synthetic Source License 1.0 (SySL)](LICENSE) — an AI-era license that operates
as a binding contract independent of copyright status.
