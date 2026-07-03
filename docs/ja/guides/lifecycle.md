# グレースフルシャットダウンとドレイン

## 問題点

ほとんどの Rust サーバーは `ctrl_c`（SIGINT）しか捕捉しません。しかし
`docker stop`、`systemctl restart`、Kubernetes のポッド終了は **SIGTERM** を
送ります —— これはグレースフルシャットダウンをバイパスし、猶予期間後に
進行中のリクエストを強制終了します。

## `DrainController`

`DrainController` は共有のドレインフラグを保持し、任意のタスクがそれを
待機できるようにします。`tokio::sync::Notify` + アトミック操作の上に
構築されています。

```rust
use malkuth::{DrainController, ShutdownKind};

let ctrl = DrainController::new();
```

## シグナルのセマンティクス

`SignalExitSource`（feature `signals`）は標準的なシグナルハンドラをインストールします：

| シグナル | `ShutdownKind` | ドレイン？ | 終了？ |
| --- | --- | --- | --- |
| `SIGINT` / `SIGTERM` | `Graceful` | はい | はい |
| `SIGHUP` | `Reload` | いいえ | いいえ（サービス継続） |
| `SIGQUIT` | `Immediate` | はい（ドレインをスキップ） | はい |

`SIGHUP` はドレインをトリガー**しません** —— リロード時には `wait_for_drain()` は
解決されません。リロードも監視したい場合は `wait_for_signal()` を使います。

## プログラムによるドレイン

プロセス内部から（例えば `Lifecycle.Drain` RPC から）ドレインをトリガーします：

```rust
ctrl.begin_drain(ShutdownKind::Graceful); // → all wait_for_drain() callers wake
```

## ドレイン状態の監視

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

## `Supervised` の使用

`Supervised` は `DrainController` + 終了ソース + ドレインフックを 1 つの
serve ループにまとめます：

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
