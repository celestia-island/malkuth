# Мягкое завершение и дрейн

## Проблема

Большинство Rust-серверов перехватывают только `ctrl_c` (SIGINT). Однако
`docker stop`, `systemctl restart` и завершение подов Kubernetes отправляют
**SIGTERM** — это обходит мягкое завершение и убивает летящие запросы после
истечения льготного периода.

## `DrainController`

`DrainController` хранит разделяемый флаг дрейна и позволяет любой задаче
ожидать его. Он построен на `tokio::sync::Notify` + атомарных операциях.

```rust
use malkuth::{DrainController, ShutdownKind};

let ctrl = DrainController::new();
```

## Семантика сигналов

`SignalExitSource` (feature `signals`) устанавливает канонические обработчики:

| Сигнал | `ShutdownKind` | Дрейн? | Выход? |
| --- | --- | --- | --- |
| `SIGINT` / `SIGTERM` | `Graceful` | Да | Да |
| `SIGHUP` | `Reload` | Нет | Нет (продолжает обслуживать) |
| `SIGQUIT` | `Immediate` | Да (пропуск дрейна) | Да |

`SIGHUP` **не** запускает дрейн — `wait_for_drain()` не разрешается при
перезагрузке. Используйте `wait_for_signal()`, если нужно наблюдать и
перезагрузки.

## Программный дрейн

Запустите дрейн изнутри процесса (например, через RPC `Lifecycle.Drain`):

```rust
ctrl.begin_drain(ShutdownKind::Graceful); // → all wait_for_drain() callers wake
```

## Наблюдение за состоянием дрейна

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

## Использование `Supervised`

`Supervised` компонует `DrainController` + источник выхода + дрейн-хуки в
единый цикл обслуживания:

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
