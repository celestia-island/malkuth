# Malkuth
<!-- markdownlint-disable MD033 MD041 MD036 -->
<div align="center">

<img src="../logo.webp" alt="Malkuth" width="200"/>


**讓長時間執行的程式自我升級並平衡負載的基礎架構**

[![License](https://img.shields.io/badge/license-BSL--1.1-blue.svg)](../../LICENSE)
[![Rust](https://img.shields.io/badge/rust-1.85%2B-orange.svg)](https://www.rust-lang.org/)
[![GitHub](https://img.shields.io/badge/github-celestia--island%2Fmalkuth-blue.svg)](https://github.com/celestia-island/malkuth)

</div>
<!-- markdownlint-enable MD033 MD041 MD036 -->

**[English](../en/README.md)** &bull; **[简体中文](../zhs/README.md)** &bull;
**[繁體中文](README.md)** &bull; **[日本語](../ja/README.md)** &bull;
**[한국어](../ko/README.md)** &bull; **[Français](../fr/README.md)** &bull;
**[Español](../es/README.md)** &bull; **[Русский](../ru/README.md)**

> **版本 0.1.0** — 早期開發階段。獨立且自包含；僅依賴 tokio + axum。

Malkuth 協助自動化、長時間執行的程式 — 常駐程式（daemons）、代理程式（agents）、伺服器（servers）— 安全地完成兩件困難的事：

- **自我升級（Self-upgrade）** — 推出新版本（或剛編譯完成的建置）而不會中斷進行中的工作或連線：零停機時間的滾動更新。
- **負載平衡（Load balancing）** — 執行多個共享工作並協調狀態的執行個體，其中一個可以優雅退役，同時由另一個接手。

## 建構區塊

- **生命週期（Lifecycle）** — 透過 `DrainController` 提供統一的信號語意（`SIGTERM` / `SIGINT` = 排空，`SIGHUP` = 重新載入，`SIGQUIT` = 立即）。
- **探針（Probes）** — 拆分 `/healthz`（存活度）與 `/readyz`（就緒度，含排空位元），讓負載平衡器與協調器能夠路由並退役節點。
- **工作器（Workers）** — 受監督的子行程資源，各自為故障隔離邊界，配備 OTP 風格的重啟策略與滑動視窗速率限制。
- **監聽器交接（Listener handoff）** — 以 socket-activation 進行監聽器繼承，並以 plain-bind 作為後備，實現零停機時間重啟。
- **協調鎖（Coordination locks）** — 一個可插拔的 `CoordinationLock` trait（`file-lock` / `pg-lock` / `lease`），用於協調並發寫入或領導者選舉。

## 快速入門

```toml
[dependencies]
malkuth = { git = "https://github.com/celestia-island/malkuth.git", branch = "dev" }
# features: socket-activation, file-lock, lease, pg-lock, replica, leader-follower
```

```rust
use malkuth::{acquire_listener, probe_router, ProbeState, DrainController};

#[tokio::main]
async fn main() -> std::io::Result<()> {
    // Listener handoff: socket activation, falls back to a plain bind.
    let listener = acquire_listener("0.0.0.0:8080").await?;

    // Probes + signal-aware drain.
    let probe = ProbeState::new(env!("CARGO_PKG_VERSION"));
    let ctrl = DrainController::install();

    let app = axum::Router::new()
        .merge(probe_router(probe)) // GET /healthz, GET /readyz
        .with_state(());

    axum::serve(listener, app)
        .with_graceful_shutdown(async {
            // Resolves on SIGINT / SIGTERM (drain) or SIGQUIT (immediate),
            // but NOT on SIGHUP (reload — the server keeps serving).
            ctrl.wait_for_drain().await;
        })
        .await?;
    Ok(())
}
```

## 功能旗標

| 功能 | 啟用 |
| --- | --- |
| `socket-activation` | 繼承監聽器 fd（socket activation） |
| `file-lock` | POSIX `flock` `CoordinationLock` 後端 |
| `lease` | 基於租約的檔案鎖，具 TTL 自動過期 |
| `pg-lock` | PostgreSQL `pg_advisory_lock` 後端（階段性） |
| `replica` | `InstanceRegistry` trait（負載平衡／滾動更新） |
| `leader-follower` | `LeaderElector` trait（主動-被動高可用性，HA） |

## 狀態

生命週期與探針、受監督的工作器、監聽器交接，以及帶有 `file-lock` 後端的協調鎖 trait 皆已實作。`replica` / `leader-follower` 策略後端為 trait 契約，完整實作尚在規劃中。設計詳見 [docs/design/](../en/design/)。

## 授權條款

商業原始碼授權 1.1（BSL-1.1）；將於 2030-01-01 自動轉換為您選擇的 Apache-2.0 或 MIT。詳見 [LICENSE](../../LICENSE)。
