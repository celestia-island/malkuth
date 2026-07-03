# 優雅關閉與排空

## 問題所在

大多數 Rust 伺服器只捕捉 `ctrl_c`（SIGINT）。但是 `docker stop`、
`systemctl restart` 以及 Kubernetes pod 終止傳送的是 **SIGTERM** —— 這會繞過
優雅關閉，並在寬限期後殺死進行中的請求。

## `DrainController`

`DrainController` 持有一個共享的排空旗標，並允許任意工作等待它。
它基於 `tokio::sync::Notify` + 原子操作構建。

```rust
use malkuth::{DrainController, ShutdownKind};

let ctrl = DrainController::new();
```

## 訊號語意

`SignalExitSource`（feature `signals`）安裝規範化的訊號處理器：

| 訊號 | `ShutdownKind` | 排空？ | 退出？ |
| --- | --- | --- | --- |
| `SIGINT` / `SIGTERM` | `Graceful` | 是 | 是 |
| `SIGHUP` | `Reload` | 否 | 否（繼續服務） |
| `SIGQUIT` | `Immediate` | 是（跳過排空） | 是 |

`SIGHUP` **不會**觸發排空 —— 重載時 `wait_for_drain()` 不會被解除阻塞。如果
你也需要觀察重載，請使用 `wait_for_signal()`。

## 程式化排空

從行程內部觸發排空（例如透過 `Lifecycle.Drain` RPC）：

```rust
ctrl.begin_drain(ShutdownKind::Graceful); // → all wait_for_drain() callers wake
```

## 觀察排空狀態

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

`Supervised` 把 `DrainController` + 退出來源 + 排空 hook 組合成一個單一的 serve
迴圈：

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
