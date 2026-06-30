# 優雅關閉與排空

## 問題所在

大多數 Rust 伺服器只捕捉 `ctrl_c`（SIGINT）。但 `docker stop`、`systemctl restart`
和 Kubernetes Pod 終止會發送 **SIGTERM** —— 這會繞過你的優雅關閉邏輯，
直接殺掉進行中的請求。

## 解決方案：`DrainController`

`DrainController::install()` 遵循 nginx/Go 的慣例，設定標準訊號處理程式：

| 訊號 | 含義 | 是否排空？ |
| --- | --- | --- |
| `SIGINT` / `SIGTERM` | 優雅關閉 | 是 |
| `SIGHUP` | 熱設定重新載入 | 否（伺服器繼續服務） |
| `SIGQUIT` | 立即退出 | 是（跳過排空） |

## 用法

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

## 接入 `axum::serve`

```rust
axum::serve(listener, app)
    .with_graceful_shutdown(async {
        ctrl.wait_for_drain().await;
    })
    .await?;
```

`wait_for_drain` 會在 `SIGINT`/`SIGTERM`/`SIGQUIT` 時完成，但**不會**在
`SIGHUP` 時完成，因此重新載入不會意外關閉伺服器。

## 觀察排空狀態

```rust
// Non-blocking check:
if ctrl.is_draining() {
    // refuse new work
}

// Sleep, but wake early if drain begins:
ctrl.sleep_or_drain(std::time::Duration::from_secs(30)).await;
```

## 程式化排空

你也可以從行程內部觸發排空（例如管理 RPC）：

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
