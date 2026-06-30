# Malkuth
<!-- markdownlint-disable MD033 MD041 MD036 -->
<div align="center">

<img src="../logo.webp" alt="Malkuth" width="200"/>


**用于长驻程序自我升级与负载均衡的基础设施**

[![License](https://img.shields.io/badge/license-BSL--1.1-blue.svg)](LICENSE)
[![Rust](https://img.shields.io/badge/rust-1.85%2B-orange.svg)](https://www.rust-lang.org/)
[![GitHub](https://img.shields.io/badge/github-celestia--island%2Fmalkuth-blue.svg)](https://github.com/celestia-island/malkuth)

</div>
<!-- markdownlint-enable MD033 MD041 MD036 -->

**[English](../en/README.md)** &bull; **[简体中文](README.md)** &bull;
**[繁體中文](../zht/README.md)** &bull; **[日本語](../ja/README.md)** &bull;
**[한국어](../ko/README.md)** &bull; **[Français](../fr/README.md)** &bull;
**[Español](../es/README.md)** &bull; **[Русский](../ru/README.md)**

> **版本 0.1.0** — 早期开发阶段。独立且自包含；仅依赖 tokio + axum。

malkuth 帮助自动化的长驻程序 —— 守护进程、代理、服务器 —— 安全地完成两件困难的事：

- **自我升级** —— 在不丢失进行中的任务或连接的情况下推出新版本（或新编译的构建产物）：零停机滚动更新。
- **负载均衡** —— 运行多个实例共享工作并协调状态，其中一个可以优雅退出，同时由另一个接管。

## 构建块

- **生命周期** —— 通过 `DrainController` 提供统一的信号语义（`SIGTERM` / `SIGINT` = 排空，`SIGHUP` = 重载，`SIGQUIT` = 立即）。
- **探针** —— 拆分 `/healthz`（存活）+ `/readyz`（就绪，带排空位），以便负载均衡器和编排器可以路由并退役节点。
- **工作进程** —— 受监管的子进程资源，每个都是故障隔离边界，带有 OTP 风格的重启策略和滑动窗口限流。
- **监听器交接** —— 带有普通绑定回退的 socket 激活监听器继承，用于零停机重启。
- **协调锁** —— 可插拔的 `CoordinationLock` trait（`file-lock` / `pg-lock` / `lease`），用于协调并发写入或领导者选举。

## 快速开始

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

## 特性开关

| Feature | 启用 |
| --- | --- |
| `socket-activation` | 继承一个监听器 fd（socket 激活） |
| `file-lock` | POSIX `flock` `CoordinationLock` 后端 |
| `lease` | 带 TTL 自动过期的基于租约的文件锁 |
| `pg-lock` | PostgreSQL `pg_advisory_lock` 后端（计划中） |
| `replica` | `InstanceRegistry` trait（负载均衡 / 滚动更新） |
| `leader-follower` | `LeaderElector` trait（主备高可用） |

## 状态

生命周期 + 探针、受监管的工作进程、监听器交接以及带 `file-lock` 后端的协调锁 trait 已经实现。`replica` / `leader-follower` 策略后端是 trait 契约，完整实现计划中。设计请参见 [docs/design/](../en/design/)。

## 许可证

Business Source License 1.1 (BSL-1.1)；将于 2030-01-01 自动转换为你选择的 Apache-2.0 或 MIT 许可证。详见 [LICENSE](../../LICENSE)。
