# Malkuth
<!-- markdownlint-disable MD033 MD041 MD036 -->
<div align="center">

<img src="docs/logo.webp" alt="Malkuth" width="200"/>

**Composable service-supervision toolkit for Rust — runtime-agnostic, framework-light, with a watchdog CLI.**

[![License](https://img.shields.io/badge/license-SySL%201.0-blue)](LICENSE)
[![Rust](https://img.shields.io/badge/rust-1.85%2B-orange.svg)](https://www.rust-lang.org/)
[![GitHub](https://img.shields.io/badge/github-celestia--island%2Fmalkuth-blue.svg)](https://github.com/celestia-island/malkuth)

</div>
<!-- markdownlint-enable MD033 MD041 MD036 -->

**[English](docs/en/README.md)** &bull; **[简体中文](docs/zhs/README.md)** &bull;
**[繁體中文](docs/zht/README.md)** &bull; **[日本語](docs/ja/README.md)** &bull;
**[한국어](docs/ko/README.md)** &bull; **[Français](docs/fr/README.md)** &bull;
**[Español](docs/es/README.md)** &bull; **[Русский](docs/ru/README.md)**

> **Version 0.2.0** — Workspace of three crates. The library runs under
> **tokio, async-std or smol** and binds to no server framework; the CLI is a
> standalone watchdog that wraps *any* program.

Malkuth helps automated, long-running programs do four hard things:

1. **Pluggable transport** — JSON-RPC over local TCP loopback, remote
   **WebSocket**, or local **IPC** (Unix sockets / named pipes via
   [`interprocess`](https://crates.io/crates/interprocess)). Nothing is forced.
2. **Runtime-agnostic core** — built on the `futures_io` traits, so the very
   *same* library code runs under tokio, async-std or smol. Pick the executor
   your app already uses.
3. **Optional, hookable facilities** — exit probes, heartbeats, probes and
   drain hooks are *traits*. Use the defaults (OS-signal exit, HTTP probes,
   cadenced heartbeats) or supply your own (e.g. trigger drain from an in-band
   "stop" command your server receives, or a parent supervisor over IPC).
4. **A watchdog CLI** — `malkuth -- <cmd>` wraps a program that does *not* use
   the library with file watching, a pod pool, and an L4 sticky reverse proxy.

## Workspace layout

| Crate | What it is |
| --- | --- |
| **`malkuth-core`** | Runtime-free **contracts**: wire types + traits (`Transport`, `WireConn`, `CoordinationLock`, `ExitSource`, `ProbeSink`, `Heartbeat`, `DrainHook`, …). Depends only on `serde`/`futures-io`/`event-listener`. |
| **`malkuth`** | Runtime **implementations**: JSON-RPC codec, server/client, transports (tcp/ws/ipc), supervised workers, probes, signals. Built on the async-* family. |
| **`malkuth-cli`** | The `malkuth` binary — pod pool + file watcher + sticky reverse proxy. Pins tokio (it's an end-user binary, not a linked library). |

## The CLI (wraps anything)

```
malkuth [--watch PATH]... [--proxy PUBLIC:LO-HI] [--pod-count N] -- <cmd> [args...]
```

Example — run 5 parallel copies of your server, have each listen on the `PORT`
env var (they self-assign 3001–3005), and front them with a sticky proxy on 3000:

```bash
malkuth --watch ./src --watch ./res \
        --proxy 3000:3000-3999 --pod-count 5 \
        -- cargo run
```

The proxy routes each **client IP** to a fixed backend via consistent hashing,
so a client keeps hitting the same pod until that pod restarts or scales down —
the basis for gray release / rolling restart. On a file change it drains and
restarts one pod at a time.

## The library (embed in your own service)

```toml
[dependencies]
malkuth = { git = "https://github.com/celestia-island/malkuth.git", branch = "dev" }
# features: tcp (default) | ws | ipc | signals | worker | axum-probe | replica | file-lock
```

```rust
use std::sync::Arc;
use malkuth::{Client, Router, Server};
use malkuth::transport::TcpTransport;
use malkuth_core::Transport;
use serde_json::json;

# async fn run() -> std::io::Result<()> {
let lis = TcpTransport.listen("tcp://127.0.0.1:0").await?;
let handler = Arc::new(
    Router::new().route("ping", |_p| Box::pin(async { Ok(json!("pong")) }))
);
Server::serve_listener(lis, handler).await
# }
```

Want the same server under async-std? Nothing changes but `#[tokio::main]` →
`#[async_std::main]`. Want drain triggered by your own logic instead of signals?
Implement `malkuth_core::ExitSource` and hand it to the supervisor.

## Feature flags (`malkuth`)

| Feature | Enables |
| --- | --- |
| `tcp` *(default)* | JSON-RPC over local/remote TCP (`async-net`) |
| `ws` | JSON-RPC over WebSocket (`async-tungstenite`) |
| `ipc` | JSON-RPC over local IPC (`interprocess`; tokio runtime) |
| `signals` | Default OS-signal `ExitSource` (`async-signal`) |
| `worker` | Supervised child-process workers (`async-process`) |
| `axum-probe` | axum `/healthz` + `/readyz` router |
| `replica` | In-memory `InstanceRegistry` |
| `file-lock` | POSIX `flock` `CoordinationLock` backend |

## Status

Layers 1–3 (lifecycle/drain, probes, listener handoff, coordination lock) and
the JSON-RPC core (codec + runtime-agnostic server/client + tcp/ws/ipc
transports) are implemented and tested end-to-end across runtimes. The CLI pod
pool + sticky proxy is working (e2e-verified). The `leader-follower` /
`pg-lock` / `lease` strategy backends remain trait contracts with full
implementations staged. See [docs/design/](docs/en/design/) for the design.

## License

[Synthetic Source License 1.0 (SySL)](LICENSE) — an AI-era license that operates
as a binding contract independent of copyright status.
