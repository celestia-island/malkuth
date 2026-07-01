# 快速开始

## 添加依赖

```toml
[dependencies]
malkuth = { git = "https://github.com/celestia-island/malkuth.git", branch = "dev" }
# features: tcp (default) | ws | ipc | signals (default) | worker | probes |
#           file-lock | lease | pg-lock | replica | leader-follower | schema
```

## 一个最小化的 JSON-RPC 服务

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

`Supervised` 让 JSON-RPC 服务器与操作系统信号退出源（SIGINT/SIGTERM →
排空，SIGHUP → 重载，SIGQUIT → 立即退出）进行竞速，然后运行所有已注册的
排空钩子。用 `.exit(your_impl)` 替换 `.signals()` 即可从你自己的逻辑触发排空。

## 从客户端调用

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

## JSON-RPC 生命周期协议

`Router::lifecycle(drain, probe)` 注册四个标准方法：

| 方法 | 参数 | 结果 | 效果 |
| --- | --- | --- | --- |
| `Lifecycle.Drain` | `{}` | `{ "accepted": true, "draining": true }` | 开始优雅排空 |
| `Lifecycle.Reload` | `{}` | `null` | 开始重载（不退出） |
| `Lifecycle.Status` | `{}` | `ReadyStatus` | 查询就绪状态（排空位 + 依赖项） |
| `Lifecycle.Health` | `{}` | `HealthStatus` | 查询存活状态（pid / 运行时间 / 版本） |

所有消息都通过选定传输层上 NDJSON 分帧的 JSON-RPC 2.0 进行传输。

## 其他传输方式

把 `TcpTransport` 换成 `WsTransport`（feature `ws`，地址 `ws://host:port`）或
`IpcTransport`（feature `ipc`，地址 `ipc:/tmp/sock`）。也可以使用
`MultiTransport`，它会根据 URL scheme（`tcp://` / `ws://` / `ipc:`）进行分派。

## 用 CLI 包装任意程序

```bash
malkuth --watch ./src --proxy 3000:3000-3999 --pod-count 3 -- cargo run
```

这会运行 3 个 pod（通过 `PORT` 环境变量自分配端口 3001–3003），逐个探测每个 pod
直到它开始监听，并用一个粘性反向代理在 3000 端口上做前端（按客户端 IP 做一致性
哈希路由）。`./src` 下的变更会触发滚动重启，一次重启一个 pod。
