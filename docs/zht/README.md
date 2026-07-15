<p align="center"><img src="https://raw.githubusercontent.com/celestia-island/docs.celestia.world/dev/res/logo/malkuth.webp" alt="Malkuth" width="240" /></p>

<h1 align="center">Malkuth</h1>

<p align="center"><strong>可組合的 Rust 服務監管工具包</strong></p>

<div align="center">

[![License: SySL-1.0](https://img.shields.io/badge/License-SySL--1.0-blue.svg)](https://sysl.celestia.world)
[![GitHub](https://img.shields.io/badge/github-celestia--island%2Fmalkuth-blue.svg)](https://github.com/celestia-island/malkuth)
[![Checks](https://img.shields.io/github/actions/workflow/status/celestia-island/malkuth/checks.yml)](https://github.com/celestia-island/malkuth/actions/workflows/checks.yml)
[![Docs](https://img.shields.io/badge/docs-malkuth.docs.celestia.world-blue)](https://malkuth.docs.celestia.world)
[![docs.rs](https://docs.rs/malkuth/badge.svg)](https://docs.rs/malkuth)

</div>

<div align="center">

[English](../en/README.md) · [简体中文](../zhs/README.md) · **繁體中文** · [日本語](../ja/README.md) · [한국어](../ko/README.md) · [Français](../fr/README.md) · [Español](../es/README.md) · [Русский](../ru/README.md) · [العربية](../ar/README.md)

</div>

Malkuth 幫助自動化、長期執行的程式完成四件難事：

1. **可插拔傳輸** —— 透過本地 TCP 回環、遠端
   **WebSocket**，或本地 **IPC**（經由
   [`interprocess`](https://crates.io/crates/interprocess) 實作的 Unix socket / 具名管道）進行 JSON-RPC 通訊。只需一個 `Transport`
   trait，依 URL scheme 分派。
2. **受監管的 worker** — 啟動處理程序、監控其健康狀態、故障時重新啟動、關閉前排空連線。
3. **選用、可掛鉤的設施** —— 退出來源、探針、心跳與排空鉤子皆為
   *trait*。使用預設實作（OS 訊號退出、axum 探針、受監管的
   worker），或提供你自己的實作（例如從你的伺服器收到的頻內「stop」命令觸發排空）。一個開箱即用的
   `Supervised` 協調器將它們串接起來。
4. **一個 watchdog 命令列工具** —— `malkuth -- <cmd>` 用檔案監看、一個
   pod 池與一個 L4 黏性反向代理來封裝程式。

## 以 CLI 使用

```
malkuth [--watch PATH]... [--proxy PUBLIC:LO-HI] [--pod-count N] -- <cmd> [args...]
```

同時執行 5 個你伺服器的平行副本（每個監聽 `PORT` 環境變數 → 它們自行指派 3001–3005），前方由一個監聽在 3000 埠的黏性代理提供服務：

```bash
malkuth --watch ./src --watch ./res \
        --proxy 3000:3000-3999 --pod-count 5 \
        -- cargo run
```

代理透過一致性雜湊將每個**客戶端 IP** 路由到固定的後端，因此客戶端會持續連接到同一個 pod，直到該 pod 重啟或縮減為止 —— 這是灰度發布／滾動重啟的基礎。當檔案變更時，它會逐個排空並重啟 pod。

## 以依賴庫使用

```toml
[dependencies]
malkuth = "0.1"
# features: tcp (default) | ws | ipc | signals (default) | worker | probes |
#           file-lock | lease | pg-lock | replica | leader-follower | schema | cli
```

```rust
use std::sync::Arc;
use malkuth::{Client, Router, Server, Supervised, Transport};
use malkuth::transport::TcpTransport;
use serde_json::json;

#[tokio::main]
async fn main() -> std::io::Result<()> {
    // Bind once; build a router with the standard lifecycle RPC + your methods.
    let lis = TcpTransport.listen("tcp://127.0.0.1:0").await?;
    let supervised = Supervised::new().signals();           // OS-signal exit source
    let ctrl = supervised.drain_controller();
    let handler = Arc::new(
        Router::new()
            .lifecycle(ctrl, None)                          // Lifecycle.Drain/Status/...
            .route("ping", |_| Box::pin(async { Ok(json!("pong")) })),
    );
    // Race the server against the exit source, then run drain hooks.
    supervised.serve_rpc_listener(lis, handler).await
}
```

需要由你自己的邏輯觸發排空而非訊號？實作 `malkuth::ExitSource` 並透過 `.exit(...)` 傳入。想要 Postgres 支援的協調？`pg-lock` 功能提供了一個 `CoordinationLock` 後端。

## 功能旗標

| 功能 | 啟用內容 |
| --- | --- |
| `tcp` *(預設)* | 基於本地／遠端 TCP 的 JSON-RPC（`tokio::net`） |
| `ws` | 基於 WebSocket 的 JSON-RPC（`tokio-tungstenite`） |
| `ipc` | 基於本地 IPC 的 JSON-RPC（`interprocess`） |
| `signals` *(預設)* | 預設的 OS 訊號 `ExitSource`（`tokio::signal`） |
| `worker` | 受監管的子行程 worker（`tokio::process`） |
| `probes` | axum `/healthz` + `/readyz` 路由器 |
| `file-lock` | POSIX `flock` `CoordinationLock` 後端（unix） |
| `lease` | 具備 TTL 自動到期的檔案租約 `CoordinationLock`（崩潰安全） |
| `pg-lock` | PostgreSQL `pg_advisory_lock` 後端（`tokio-postgres`） |
| `replica` | 記憶體內的 `InstanceRegistry` |
| `leader-follower` | `LeaseLeaderElector`（基於租約後端） |
| `schema` | 針對線路類型的 `schemars::JsonSchema` derive |
| `cli` | `malkuth` watchdog 二進位檔（pod 池 + 黏性代理） |

## 狀態

第 1–3 層（生命週期／排空、探針、監聽器交接）以及 JSON-RPC 核心
（編解碼器 + 伺服器／客戶端 + tcp/ws/ipc 傳輸）已實作完成並通過端對端測試。命令列工具的 pod 池 + 黏性代理已可運作（經端對端驗證）。全部三種
`CoordinationLock` 後端（`file-lock`、`lease`、`pg-lock`）以及
`leader-follower` `LeaseLeaderElector` 皆已實作。設計請參閱
[docs/design/](../en/design/)。

## MCP 伺服器

使用 `mcp` feature 建置 malkuth 並執行 stdio 伺服器——它透過模型上下文協定（Model Context Protocol）將監管工具包暴露給 AI 編碼助手：

```bash
malkuth mcp
```

伺服器提供兩個工具：`malkuth_supervise`（在監管器下以重啟策略 + 滑動窗口速率限制啟動一組 worker；阻塞直到它們退出或超時觸發，然後回傳最終狀態快照）和 `malkuth_probe`（對服務 URL 進行 HTTP healthz / readyz 檢查）。將其接入 MCP 客戶端：

```json
{
  "mcpServers": {
    "malkuth": { "command": "malkuth", "args": ["mcp"] }
  }
}
```

`mcp` feature 隱含 `worker` + `schema`；它還加入了 `rmcp` 和用於探測工具的 `reqwest` 客戶端。

## 授權條款

SySL-1.0（Synthetic Source License）。詳見 [LICENSE](https://sysl.celestia.world)。
