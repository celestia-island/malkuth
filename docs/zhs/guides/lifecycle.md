# 优雅关闭与排空

## 问题所在

大多数 Rust 服务器只捕获 `ctrl_c`（SIGINT）。但是 `docker stop`、
`systemctl restart` 以及 Kubernetes pod 终止发送的是 **SIGTERM** —— 这会绕过
优雅关闭，并在宽限期后杀死进行中的请求。

## `DrainController`

`DrainController` 持有一个共享的排空标志位，并允许任意任务等待它。
它基于 `tokio::sync::Notify` + 原子操作构建。

```rust
use malkuth::{DrainController, ShutdownKind};

let ctrl = DrainController::new();
```

## 信号语义

`SignalExitSource`（feature `signals`）安装规范化的信号处理器：

| 信号 | `ShutdownKind` | 排空？ | 退出？ |
| --- | --- | --- | --- |
| `SIGINT` / `SIGTERM` | `Graceful` | 是 | 是 |
| `SIGHUP` | `Reload` | 否 | 否（继续服务） |
| `SIGQUIT` | `Immediate` | 是（跳过排空） | 是 |

`SIGHUP` **不会**触发排空 —— 重载时 `wait_for_drain()` 不会被解除阻塞。如果
你也需要观察重载，请使用 `wait_for_signal()`。

## 编程式排空

从进程内部触发排空（例如通过 `Lifecycle.Drain` RPC）：

```rust
ctrl.begin_drain(ShutdownKind::Graceful); // → all wait_for_drain() callers wake
```

## 观察排空状态

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

## 使用 `Supervised`

`Supervised` 把 `DrainController` + 退出源 + 排空钩子组合成一个单一的 serve
循环：

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
