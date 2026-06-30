# グレースフルシャットダウンとドレイン

## 問題点

ほとんどの Rust サーバーは `ctrl_c`（SIGINT）しか捕捉しません。しかし、`docker stop`、`systemctl restart`、
Kubernetes の Pod 終了は **SIGTERM** を送信します —— これはグレースフルシャットダウンをバイパスし、
処理中のリクエストを強制終了してしまいます。

## 解決策：`DrainController`

`DrainController::install()` は nginx/Go の慣例に従い、標準的なシグナルハンドラを設定します：

| シグナル | 意味 | ドレインするか |
| --- | --- | --- |
| `SIGINT` / `SIGTERM` | グレースフルシャットダウン | する |
| `SIGHUP` | 設定のホットリロード | しない（サーバーはサービスを継続） |
| `SIGQUIT` | 即時終了 | する（ドレインをスキップ） |

## 使い方

```rust
use malkuth::DrainController;

let ctrl = DrainController::install();

// Pass clones to whoever needs to observe drain:
// - the serve loop (to stop accepting)
// - the probe layer (to set the /readyz draining bit)
// - background tasks (to wind down)

// Block until a drain/immediate signal fires.
let kind = ctrl.wait_for_drain().await;
```

## `axum::serve` への組み込み

```rust
axum::serve(listener, app)
    .with_graceful_shutdown(async {
        ctrl.wait_for_drain().await;
    })
    .await?;
```

`wait_for_drain` は `SIGINT`/`SIGTERM`/`SIGQUIT` で完了しますが、`SIGHUP` では完了**しません**。
したがって、リロードによって誤ってサーバーがシャットダウンされることはありません。

## ドレイン状態の監視

```rust
// Non-blocking check:
if ctrl.is_draining() {
    // refuse new work
}

// Sleep, but wake early if drain begins:
ctrl.sleep_or_drain(std::time::Duration::from_secs(30)).await;
```

## プログラムによるドレイン

プロセス内部からドレインをトリガーすることもできます（例：管理用 RPC）：

```rust
ctrl.begin_drain(malkuth::ShutdownKind::Graceful);
```

## `ShutdownKind`

```rust
pub enum ShutdownKind {
    Graceful,   // SIGINT / SIGTERM — drain, then exit 0
    Immediate,  // SIGQUIT — skip drain, exit fast
    Reload,     // SIGHUP — reload config, do NOT exit
}
```
