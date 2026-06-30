# 快速入門

## 將 malkuth 加入你的專案

```toml
[dependencies]
malkuth = { git = "https://github.com/celestia-island/malkuth.git", branch = "dev" }
# Optional features:
#   socket-activation  — inherit a listener fd from systemd
#   file-lock          — POSIX flock coordination-lock backend
#   lease              — lease-based file lock with TTL auto-expiry
#   replica            — InstanceRegistry trait (load-balancing)
#   leader-follower    — LeaderElector trait (active-passive HA)
```

## 具備優雅關閉與探針的最小伺服器

```rust
use malkuth::{acquire_listener, probe_router, ProbeState, DrainController};

#[tokio::main]
async fn main() -> std::io::Result<()> {
    // 1. Acquire a listener — prefers systemd socket activation,
    //    falls back to binding the given address.
    let listener = acquire_listener("0.0.0.0:8080").await?;

    // 2. Create probe state and install the drain controller.
    let probe = ProbeState::new(env!("CARGO_PKG_VERSION"));
    let ctrl = DrainController::install();

    // 3. Build your router, merging the probe routes.
    let app = axum::Router::new()
        .route("/", axum::routing::get(|| async { "hello" }))
        .merge(probe_router(probe))
        .with_state(());

    // 4. Serve with graceful shutdown: SIGINT/SIGTERM trigger drain,
    //    SIGQUIT forces immediate exit, SIGHUP reloads (keeps serving).
    axum::serve(listener, app)
        .with_graceful_shutdown(async {
            ctrl.wait_for_drain().await;
        })
        .await?;
    Ok(())
}
```

## 你會獲得什麼

| 端點 | 用途 |
| --- | --- |
| `GET /healthz` | 存活度 ——「行程還活著」（pid、運行時間、版本） |
| `GET /readyz` | 就緒度 ——「可以服務」（排空期間回傳 503） |

| 訊號 | 行為 |
| --- | --- |
| `SIGINT` / `SIGTERM` | 優雅排空（完成進行中的請求，然後退出） |
| `SIGHUP` | 熱重新載入（**不會**退出 —— 伺服器繼續服務） |
| `SIGQUIT` | 立即退出（僅限緊急情況） |

## 功能旗標

| 功能 | 啟用 |
| --- | --- |
| `socket-activation` | 從 systemd 繼承監聽器 fd（零停機重啟） |
| `file-lock` | 基於 POSIX `flock` 的 `CoordinationLock` 後端 |
| `lease` | 崩潰時具 TTL 自動過期的基於租約的檔案鎖 |
| `replica` | 用於負載平衡副本的 `InstanceRegistry` trait |
| `leader-follower` | 用於主動-被動高可用性的 `LeaderElector` trait |

所有功能均為可選；預設建置不包含任何 unsafe 程式碼，僅依賴 tokio + axum。
