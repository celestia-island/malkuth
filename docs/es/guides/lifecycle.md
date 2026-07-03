# Apagado gracioso y drenaje

## El problema

La mayoría de servidores Rust solo capturan `ctrl_c` (SIGINT). Pero
`docker stop`, `systemctl restart` y la terminación de pods de Kubernetes envían
**SIGTERM** — lo que elude el apagado gracioso y mata las solicitudes en vuelo
tras el periodo de gracia.

## `DrainController`

`DrainController` mantiene una bandera de drenaje compartida y permite que
cualquier tarea la espere. Se construye sobre `tokio::sync::Notify` + atómicos.

```rust
use malkuth::{DrainController, ShutdownKind};

let ctrl = DrainController::new();
```

## Semántica de señales

`SignalExitSource` (feature `signals`) instala manejadores canónicos:

| Señal | `ShutdownKind` | ¿Drena? | ¿Termina? |
| --- | --- | --- | --- |
| `SIGINT` / `SIGTERM` | `Graceful` | Sí | Sí |
| `SIGHUP` | `Reload` | No | No (sigue sirviendo) |
| `SIGQUIT` | `Immediate` | Sí (omite el drenaje) | Sí |

`SIGHUP` **no** dispara el drenaje — `wait_for_drain()` no se resuelve ante una
recarga. Usa `wait_for_signal()` si también necesitas observar las recargas.

## Drenaje programático

Dispara el drenaje desde dentro del proceso (por ejemplo, desde un RPC
`Lifecycle.Drain`):

```rust
ctrl.begin_drain(ShutdownKind::Graceful); // → all wait_for_drain() callers wake
```

## Observar el estado de drenaje

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

## Usar `Supervised`

`Supervised` compone `DrainController` + una fuente de salida + hooks de drenaje
en un único bucle de servicio:

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
