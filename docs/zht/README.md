<!-- markdownlint-disable MD033 MD041 MD036 -->
<div align="center">

<img src="../res/logo.webp" alt="Plana logo" width="200"/>

# Plana

**幫助長執行的程式完成自我升級與負載均衡的基礎設施**

[![License](https://img.shields.io/badge/license-BSL--1.1-blue.svg)](../LICENSE)
[![Rust](https://img.shields.io/badge/rust-1.85%2B-orange.svg)](https://www.rust-lang.org/)
[![GitHub](https://img.shields.io/badge/github-celestia--island%2Fplana-blue.svg)](https://github.com/celestia-island/plana)

</div>
<!-- markdownlint-enable MD033 MD041 MD036 -->

**[English](../../README.md)** &bull; **[簡體中文](README.md)** &bull;
**[繁體中文](../zht/README.md)** &bull; **[日本語](../ja/README.md)** &bull;
**[한국어](../ko/README.md)** &bull; **[Français](../fr/README.md)** &bull;
**[Español](../es/README.md)** &bull; **[Русский](../ru/README.md)**

> **版本 0.1.0** — 早期開發。獨立自包含,僅依賴 tokio + axum。

Plana 幫助長執行的自動化程式——守護程序、agent、服務端——安全地完成兩件難事:

- **自我升級** —— 上線新版本(或新編譯的構建)而不丟失在途任務或連線:零停機滾動更新。
- **負載均衡** —— 多個例項分擔任務、協調狀態,其中一個可優雅退出而另一個接管。

## 構建塊

- **生命週期** —— 統一訊號語義(`SIGTERM` / `SIGINT` = 排空,`SIGHUP` = 過載,`SIGQUIT` = 立即),透過 `DrainController`。
- **探針** —— 分離的 `/healthz`(存活)+ `/readyz`(就緒,含排空位),讓負載均衡器和編排器能路由與摘除節點。
- **Worker** —— 被監督的子程序資源,每個是一個故障隔離邊界,帶 OTP 風格重啟策略和滑動視窗限頻。
- **監聽交接** —— socket activation 監聽繼承 + 普通 bind 回退,實現零停機重啟。
- **協調鎖** —— 可插拔的 `CoordinationLock` trait(`file-lock` / `pg-lock` / `lease`),用於協調併發寫或選主。

## 快速開始

```toml
[dependencies]
plana = { git = "https://github.com/celestia-island/plana.git", branch = "dev" }
# 特性: socket-activation, file-lock, lease, pg-lock, replica, leader-follower
```

```rust
use plana::{acquire_listener, probe_router, ProbeState, DrainController};

#[tokio::main]
async fn main() -> std::io::Result<()> {
    // 監聽交接: socket activation,回退到普通 bind。
    let listener = acquire_listener("0.0.0.0:8080").await?;

    // 探針 + 訊號感知的排空。
    let probe = ProbeState::new(env!("CARGO_PKG_VERSION"));
    let ctrl = DrainController::install();

    let app = axum::Router::new()
        .merge(probe_router(probe)) // GET /healthz, GET /readyz
        .with_state(());

    axum::serve(listener, app)
        .with_graceful_shutdown(async {
            // 在 SIGINT / SIGTERM(排空)或 SIGQUIT(立即)時完成,
            // 但 SIGHUP(過載)不會 —— 服務繼續執行。
            ctrl.wait_for_drain().await;
        })
        .await?;
    Ok(())
}
```

## 特性開關

| 特性 | 啟用 |
| --- | --- |
| `socket-activation` | 繼承監聽 fd(socket activation) |
| `file-lock` | POSIX `flock` `CoordinationLock` 後端 |
| `lease` | 帶租約的檔案鎖,崩潰自動過期 |
| `pg-lock` | PostgreSQL `pg_advisory_lock` 後端(待實現) |
| `replica` | `InstanceRegistry` trait(負載均衡 / 滾動更新) |
| `leader-follower` | `LeaderElector` trait(主動-被動 HA) |

## 狀態

生命週期 + 探針、被監督的 worker、監聽交接,以及帶 `file-lock` 後端的協調鎖 trait 已實現。`replica` / `leader-follower` 策略後端是 trait 契約,完整實現待補。

## 許可證

Business Source License 1.1 (BSL-1.1);於 2030-01-01 自動轉換為你選擇的 Apache-2.0 或 MIT。見 [LICENSE](../LICENSE)。
