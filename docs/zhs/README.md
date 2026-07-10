<p align="center"><img src="https://raw.githubusercontent.com/celestia-island/malkuth/master/docs/logo.webp" alt="Malkuth" width="240" /></p>

<h1 align="center">Malkuth</h1>

<p align="center"><strong>可组合的 Rust 服务监管工具包</strong></p>

<div align="center">

[![License: SySL-1.0](https://img.shields.io/badge/License-SySL--1.0-blue.svg)](https://sysl.celestia.world)
[![GitHub](https://img.shields.io/badge/github-celestia--island%2Fmalkuth-blue.svg)](https://github.com/celestia-island/malkuth)
[![Checks](https://img.shields.io/github/actions/workflow/status/celestia-island/malkuth/checks.yml)](https://github.com/celestia-island/malkuth/actions/workflows/checks.yml)
[![Docs](https://img.shields.io/badge/docs-malkuth.docs.celestia.world-blue)](https://malkuth.docs.celestia.world)
[![docs.rs](https://docs.rs/malkuth/badge.svg)](https://docs.rs/malkuth)

</div>

<div align="center">

[English](../en/README.md) · **简体中文** · [繁體中文](../zht/README.md) · [日本語](../ja/README.md) · [한국어](../ko/README.md) · [Français](../fr/README.md) · [Español](../es/README.md) · [Русский](../ru/README.md) · [العربية](../ar/README.md)

</div>

Malkuth 帮助自动化、长期运行的程序完成四件难事：

1. **可插拔传输** —— 基于 JSON-RPC 的本地 TCP 回环、远程
   **WebSocket**，或本地 **IPC**（通过
   [`interprocess`](https://crates.io/crates/interprocess) 实现的 Unix 套接字 / 命名管道）。只需一个 `Transport`
   trait，按 URL scheme 分发。
2. **受监管 worker** — 启动进程、监控其健康状态、故障时重启、关闭前排空连接。
3. **可选、可挂钩的设施** —— 退出源、探针、心跳和排空钩子是
   *trait*。使用默认实现（操作系统信号退出、axum 探针、受监管
   worker），或提供你自己的实现（例如，从你的服务器接收的带内"停止"命令触发排空）。
   一个开箱即用的 `Supervised` 编排器将它们串联起来。
4. **一个 watchdog 命令行工具** —— `malkuth -- <cmd>` 用文件监视、一个
   pod 池与一个 L4 粘性反向代理来封装程序。

## 以 CLI 使用

```
malkuth [--watch PATH]... [--proxy PUBLIC:LO-HI] [--pod-count N] -- <cmd> [args...]
```

运行 5 个并行的服务器副本（每个监听 `PORT` 环境变量 → 它们自动分配 3001–3005），
前面有一个粘性代理监听在 3000 端口：

```bash
malkuth --watch ./src --watch ./res \
        --proxy 3000:3000-3999 --pod-count 5 \
        -- cargo run
```

代理通过一致性哈希将每个**客户端 IP** 路由到固定的后端，因此客户端在
pod 重启或缩容之前会一直访问同一个 pod —— 这是灰度发布 / 滚动重启的
基础。文件变更时，它会一次排空并重启一个 pod。

## 以依赖库使用

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

需要由你自己的逻辑而非信号来触发排空？实现 `malkuth::ExitSource`
并通过 `.exit(...)` 传入。想要基于 Postgres 的协调？`pg-lock` feature
提供了一个 `CoordinationLock` 后端。

## Feature 标志

| Feature | 启用功能 |
| --- | --- |
| `tcp` *(默认)* | 基于本地/远程 TCP 的 JSON-RPC（`tokio::net`） |
| `ws` | 基于 WebSocket 的 JSON-RPC（`tokio-tungstenite`） |
| `ipc` | 基于本地 IPC 的 JSON-RPC（`interprocess`） |
| `signals` *(默认)* | 默认操作系统信号 `ExitSource`（`tokio::signal`） |
| `worker` | 受监管的子进程 worker（`tokio::process`） |
| `probes` | axum `/healthz` + `/readyz` 路由 |
| `file-lock` | POSIX `flock` `CoordinationLock` 后端（unix） |
| `lease` | 具有 TTL 自动过期的文件租约 `CoordinationLock`（崩溃安全） |
| `pg-lock` | PostgreSQL `pg_advisory_lock` 后端（`tokio-postgres`） |
| `replica` | 内存 `InstanceRegistry` |
| `leader-follower` | `LeaseLeaderElector`（基于租约后端） |
| `schema` | 为传输类型派生 `schemars::JsonSchema` |
| `cli` | `malkuth` watchdog 二进制文件（pod 池 + 粘性代理） |

## 状态

第 1–3 层（生命周期/排空、探针、监听器移交）和 JSON-RPC 核心
（编解码器 + 服务器/客户端 + tcp/ws/ipc 传输）已实现并经过端到端测试。
CLI 的 pod 池 + 粘性代理已可用（经端到端验证）。所有三个
`CoordinationLock` 后端（`file-lock`、`lease`、`pg-lock`）和
`leader-follower` `LeaseLeaderElector` 已实现。设计文档见
[设计文档](../en/design/)。

## MCP 服务器

使用 `mcp` feature 构建 malkuth 并运行 stdio 服务器——它通过模型上下文协议（Model Context Protocol）将监管工具包暴露给 AI 编码助手：

```bash
malkuth mcp
```

服务器提供两个工具：`malkuth_supervise`（在监管器下以重启策略 + 滑动窗口速率限制启动一组 worker；阻塞直到它们退出或超时触发，然后返回最终状态快照）和 `malkuth_probe`（对服务 URL 进行 HTTP healthz / readyz 检查）。将其接入 MCP 客户端：

```json
{
  "mcpServers": {
    "malkuth": { "command": "malkuth", "args": ["mcp"] }
  }
}
```

`mcp` feature 隐含 `worker` + `schema`；它还添加了 `rmcp` 和用于探测工具的 `reqwest` 客户端。

## 许可证

SySL-1.0（Synthetic Source License）。详见 [LICENSE](https://sysl.celestia.world)。
