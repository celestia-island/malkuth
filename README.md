<p align="center"><img src="https://raw.githubusercontent.com/celestia-island/malkuth/master/docs/logo.webp" alt="Malkuth" width="240" /></p>

<h1 align="center">Malkuth</h1>

<p align="center"><strong>Composable service-supervision toolkit for Rust</strong></p>

<div align="center">

[![License: SySL-1.0](https://img.shields.io/badge/License-SySL--1.0-blue.svg)](https://sysl.celestia.world)
[![GitHub](https://img.shields.io/badge/github-celestia--island%2Fmalkuth-blue.svg)](https://github.com/celestia-island/malkuth)
[![Checks](https://img.shields.io/github/actions/workflow/status/celestia-island/malkuth/checks.yml)](https://github.com/celestia-island/malkuth/actions/workflows/checks.yml)
[![Docs](https://img.shields.io/badge/docs-malkuth.docs.celestia.world-blue)](https://malkuth.docs.celestia.world)
[![docs.rs](https://docs.rs/malkuth/badge.svg)](https://docs.rs/malkuth)

</div>

<div align="center">

**English** ·
[简体中文](./docs/zhs/README.md) ·
[繁體中文](./docs/zht/README.md) ·
[日本語](./docs/ja/README.md) ·
[한국어](./docs/ko/README.md) ·
[Français](./docs/fr/README.md) ·
[Español](./docs/es/README.md) ·
[Русский](./docs/ru/README.md) ·
[العربية](./docs/ar/README.md)

</div>

Malkuth helps automated, long-running programs handle supervision —
graceful shutdown, health probes, coordination locks, and rolling updates:

1. **Pluggable transport** — JSON-RPC over TCP, WebSocket, or IPC (Unix sockets /
   named pipes). One `Transport` trait, dispatched by URL scheme.
2. **Supervised workers** — spawn a process, monitor its health, restart it on
   failure, drain connections before shutdown.
3. **Optional facilities** — exit source, probes, heartbeat and drain hooks are
   *traits*. Use the defaults or supply your own.
4. **A watchdog CLI** — `malkuth -- <cmd>` wraps any program with file watching,
   a pod pool, and an L4 sticky reverse proxy.

## As a CLI

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

## As a library

```toml
[dependencies]
malkuth = "0.1"
# features: tcp (default) | ws | ipc | signals (default) | worker | probes |
#           file-lock | lease | pg-lock | replica | leader-follower | schema | cli
```

```rust
use std::sync::Arc;
use malkuth::{Client, Router, Server, Supervised, Transport};
use malkuth::transport::TcpTransport;
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
`malkuth::ExitSource` and pass it via `.exit(...)`. Want Postgres-backed
coordination? The `pg-lock` feature provides a `CoordinationLock` backend.

## Feature flags

| Feature | Enables |
| --- | --- |
| `tcp` *(default)* | JSON-RPC over local/remote TCP (`tokio::net`) |
| `ws` | JSON-RPC over WebSocket (`tokio-tungstenite`) |
| `ipc` | JSON-RPC over local IPC (`interprocess`) |
| `signals` *(default)* | Default OS-signal `ExitSource` (`tokio::signal`) |
| `worker` | Supervised child-process workers (`tokio::process`) |
| `probes` | axum `/healthz` + `/readyz` router |
| `file-lock` | POSIX `flock` `CoordinationLock` backend (unix) |
| `lease` | File-lease `CoordinationLock` with TTL auto-expiry (crash-safe) |
| `pg-lock` | PostgreSQL `pg_advisory_lock` backend (`tokio-postgres`) |
| `replica` | In-memory `InstanceRegistry` |
| `leader-follower` | `LeaseLeaderElector` (over the lease backend) |
| `schema` | `schemars::JsonSchema` derives for wire types |
| `cli` | The `malkuth` watchdog binary (pod pool + sticky proxy) |

## Status

Layers 1–3 (lifecycle/drain, probes, listener handoff) and the JSON-RPC core
(codec + server/client + tcp/ws/ipc transports) are implemented and tested
end-to-end. The CLI pod pool + sticky proxy is working (e2e-verified). All three
`CoordinationLock` backends (`file-lock`, `lease`, `pg-lock`) and the
`leader-follower` `LeaseLeaderElector` are implemented. See
[docs/design/](docs/en/design/) for the design.

## License

SySL-1.0 (Synthetic Source License). See [LICENSE](https://sysl.celestia.world).
