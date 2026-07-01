# 快速開始

## 新增依賴

```toml
[dependencies]
malkuth = { git = "https://github.com/celestia-island/malkuth.git", branch = "dev" }
# features: tcp (default) | ws | ipc | signals (default) | worker | probes |
#           file-lock | lease | pg-lock | replica | leader-follower | schema
```

## 一個最小化的 JSON-RPC 服務

```rust
use std::sync::Arc;
use malkuth::{Router, Supervised};
use malkuth::transport::TcpTransport;
use malkuth::Transport;
use serde_json::json;

#[tokio::main]
async fn main() -> std::io::Result<()> {
    let lis = TcpTransport.listen("tcp://127.0.0.1:0").await?;
    let supervised = Supervised::new().signals();
    let ctrl = supervised.drain_controller();

    let handler = Arc::new(
        Router::new()
            .lifecycle(ctrl, None)            // registers Lifecycle.Drain / Status / Health / Reload
            .route("ping", |_| Box::pin(async { Ok(json!("pong")) })),
    );

    supervised.serve_rpc_listener(lis, handler).await
}
```

`Supervised` 讓 JSON-RPC 伺服器與作業系統訊號退出來源（SIGINT/SIGTERM →
排空，SIGHUP → 重載，SIGQUIT → 立即退出）進行競速，然後執行所有已註冊的
排空 hook。用 `.exit(your_impl)` 取代 `.signals()` 即可從你自己的邏輯觸發排空。

## 從客戶端呼叫

```rust
use malkuth::Client;
use malkuth::transport::TcpTransport;
use malkuth::Transport;
use serde_json::json;

let mut c = Client::connect(&TcpTransport, "tcp://127.0.0.1:8080").await?;

// Custom method:
let r = c.call("ping", json!({})).await?;       // → "pong"

// Standard lifecycle methods (registered by Router::lifecycle):
c.notify("Lifecycle.Drain", json!({})).await?;  // → server begins graceful drain
let health = c.call("Lifecycle.Health", json!({})).await?;
// → { "alive": true, "pid": 12345, "uptime_secs": 360, "version": "0.1.0" }
let status = c.call("Lifecycle.Status", json!({})).await?;
// → { "ready": true, "draining": false, "dependencies": [], "generation": null }
```

## JSON-RPC 生命週期協定

`Router::lifecycle(drain, probe)` 註冊四個標準方法：

| 方法 | 參數 | 結果 | 效果 |
| --- | --- | --- | --- |
| `Lifecycle.Drain` | `{}` | `{ "accepted": true, "draining": true }` | 開始優雅排空 |
| `Lifecycle.Reload` | `{}` | `null` | 開始重載（不退出） |
| `Lifecycle.Status` | `{}` | `ReadyStatus` | 查詢就緒狀態（排空位元 + 相依項） |
| `Lifecycle.Health` | `{}` | `HealthStatus` | 查詢存活狀態（pid / 運行時間 / 版本） |

所有訊息都透過選定傳輸層上 NDJSON 分幀的 JSON-RPC 2.0 進行傳輸。

## 其他傳輸方式

把 `TcpTransport` 換成 `WsTransport`（feature `ws`，位址 `ws://host:port`）或
`IpcTransport`（feature `ipc`，位址 `ipc:/tmp/sock`）。也可以使用
`MultiTransport`，它會根據 URL scheme（`tcp://` / `ws://` / `ipc:`）進行分派。

## 用 CLI 包裝任意程式

```bash
malkuth --watch ./src --proxy 3000:3000-3999 --pod-count 3 -- cargo run
```

這會執行 3 個 pod（透過 `PORT` 環境變數自配置埠 3001–3003），逐一探測每個 pod
直到它開始監聽，並用一個黏性反向代理在 3000 埠上做前端（依客戶端 IP 做一致性
雜湊路由）。`./src` 下的變更會觸發滾動重啟，一次重啟一個 pod。
