# Quick Start

## Add the dependency

```toml
[dependencies]
malkuth = { git = "https://github.com/celestia-island/malkuth.git", branch = "dev" }
# features: tcp (default) | ws | ipc | signals (default) | worker | probes |
#           file-lock | lease | pg-lock | replica | leader-follower | schema
```

## A minimal JSON-RPC service

```rust
use std::sync::Arc;
use malkuth::{Router, Supervised};
use malkuth::transport::TcpTransport;
use malkuth::Transport;
use serde_json::json;

#[tokio::main]
async fn main() -> std::io::Result<()> {
    let lis = TcpTransport.listen("tcp://127.0.0.1:0").await?;
    let supervised = Supervised::new().signals();
    let ctrl = supervised.drain_controller();

    let handler = Arc::new(
        Router::new()
            .lifecycle(ctrl, None)            // registers Lifecycle.Drain / Status / Health / Reload
            .route("ping", |_| Box::pin(async { Ok(json!("pong")) })),
    );

    supervised.serve_rpc_listener(lis, handler).await
}
```

`Supervised` races the JSON-RPC server against the OS-signal exit source
(SIGINT/SIGTERM → drain, SIGHUP → reload, SIGQUIT → immediate exit), then runs
any registered drain hooks. Replace `.signals()` with `.exit(your_impl)` to
trigger drain from your own logic.

## Call it from a client

```rust
use malkuth::Client;
use malkuth::transport::TcpTransport;
use malkuth::Transport;
use serde_json::json;

let mut c = Client::connect(&TcpTransport, "tcp://127.0.0.1:8080").await?;

// Custom method:
let r = c.call("ping", json!({})).await?;       // → "pong"

// Standard lifecycle methods (registered by Router::lifecycle):
c.notify("Lifecycle.Drain", json!({})).await?;  // → server begins graceful drain
let health = c.call("Lifecycle.Health", json!({})).await?;
// → { "alive": true, "pid": 12345, "uptime_secs": 360, "version": "0.1.0" }
let status = c.call("Lifecycle.Status", json!({})).await?;
// → { "ready": true, "draining": false, "dependencies": [], "generation": null }
```

## JSON-RPC lifecycle protocol

`Router::lifecycle(drain, probe)` registers four standard methods:

| Method | Params | Result | Effect |
| --- | --- | --- | --- |
| `Lifecycle.Drain` | `{}` | `{ "accepted": true, "draining": true }` | Begin graceful drain |
| `Lifecycle.Reload` | `{}` | `null` | Begin reload (no exit) |
| `Lifecycle.Status` | `{}` | `ReadyStatus` | Query readiness (drain bit + deps) |
| `Lifecycle.Health` | `{}` | `HealthStatus` | Query liveness (pid / uptime / version) |

All messages are NDJSON-framed JSON-RPC 2.0 over the chosen transport.

## Other transports

Swap `TcpTransport` for `WsTransport` (feature `ws`, address `ws://host:port`)
or `IpcTransport` (feature `ipc`, address `ipc:/tmp/sock`). Or use
`MultiTransport`, which dispatches by URL scheme (`tcp://` / `ws://` / `ipc:`).

## Wrap any program with the CLI

```bash
malkuth --watch ./src --proxy 3000:3000-3999 --pod-count 3 -- cargo run
```

This runs 3 pods (self-assigning ports 3001–3003 via the `PORT` env var),
probes each until it is listening, and fronts them with a sticky reverse proxy
on port 3000 (consistent-hash routing by client IP). A change under `./src`
triggers a rolling restart, one pod at a time.
