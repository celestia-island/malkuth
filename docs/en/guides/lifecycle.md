# Graceful Shutdown & Drain

## The problem

Most Rust servers only catch `ctrl_c` (SIGINT). But `docker stop`,
`systemctl restart`, and Kubernetes pod termination send **SIGTERM** — which
bypasses graceful shutdown and kills in-flight requests after the grace period.

## `DrainController`

`DrainController` holds a shared drain flag and lets any task wait for it.
It is built on `tokio::sync::Notify` + atomics.

```rust
use malkuth::{DrainController, ShutdownKind};

let ctrl = DrainController::new();
```

## Signal semantics

The `SignalExitSource` (feature `signals`) installs canonical handlers:

| Signal | `ShutdownKind` | Drains? | Exits? |
| --- | --- | --- | --- |
| `SIGINT` / `SIGTERM` | `Graceful` | Yes | Yes |
| `SIGHUP` | `Reload` | No | No (keep serving) |
| `SIGQUIT` | `Immediate` | Yes (skip drain) | Yes |

`SIGHUP` does **not** trigger drain — `wait_for_drain()` does not resolve on
reload. Use `wait_for_signal()` if you need to observe reloads too.

## Programmatic drain

Trigger drain from inside the process (e.g. from a `Lifecycle.Drain` RPC):

```rust
ctrl.begin_drain(ShutdownKind::Graceful); // → all wait_for_drain() callers wake
```

## Observing drain state

```rust
// Non-blocking check:
if ctrl.is_draining() {
    // refuse new work
}

// Which kind fired?
if let Some(kind) = ctrl.kind() {
    println!("shutdown kind: {kind:?}");
}

// Async wait (resolves on Graceful or Immediate, NOT on Reload):
let kind = ctrl.wait_for_drain().await;

// Async wait for any signal (including Reload):
let kind = ctrl.wait_for_signal().await;
```

## Using `Supervised`

`Supervised` composes `DrainController` + an exit source + drain hooks into a
single serve loop:

```rust
use malkuth::{Supervised, Router};
use malkuth::transport::TcpTransport;
use malkuth::Transport;
use std::sync::Arc;

let supervised = Supervised::new()
    .signals()                          // install OS signal handler
    .on_drain(MyDrainHook)              // run cleanup during shutdown
    .drain_budget(std::time::Duration::from_secs(30));

let ctrl = supervised.drain_controller();
let handler = Arc::new(
    Router::new().lifecycle(ctrl, None)
);

// Serve JSON-RPC until a signal fires, then run drain hooks:
supervised
    .serve_rpc(&TcpTransport, "tcp://0.0.0.0:8080", handler)
    .await?;
```

## `ShutdownKind`

```rust
pub enum ShutdownKind {
    Graceful,   // SIGINT / SIGTERM — drain, then exit 0
    Immediate,  // SIGQUIT — skip drain, exit fast
    Reload,     // SIGHUP — reload config, do NOT exit
}
```
