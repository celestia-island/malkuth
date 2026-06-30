<!-- markdownlint-disable MD033 MD041 MD036 -->
<div align="center">

<img src="../res/logo.webp" alt="Plana logo" width="200"/>

# Plana

**帮助长运行的程序完成自我升级与负载均衡的基础设施**

[![License](https://img.shields.io/badge/license-BSL--1.1-blue.svg)](../LICENSE)
[![Rust](https://img.shields.io/badge/rust-1.85%2B-orange.svg)](https://www.rust-lang.org/)
[![GitHub](https://img.shields.io/badge/github-celestia--island%2Fplana-blue.svg)](https://github.com/celestia-island/plana)

</div>
<!-- markdownlint-enable MD033 MD041 MD036 -->

**[English](../../README.md)** &bull; **[简体中文](README.md)** &bull;
**[繁體中文](../zht/README.md)** &bull; **[日本語](../ja/README.md)** &bull;
**[한국어](../ko/README.md)** &bull; **[Français](../fr/README.md)** &bull;
**[Español](../es/README.md)** &bull; **[Русский](../ru/README.md)**

> **版本 0.1.0** — 早期开发。独立自包含,仅依赖 tokio + axum。

Plana 帮助长运行的自动化程序——守护进程、agent、服务端——安全地完成两件难事:

- **自我升级** —— 上线新版本(或新编译的构建)而不丢失在途任务或连接:零停机滚动更新。
- **负载均衡** —— 多个实例分担任务、协调状态,其中一个可优雅退出而另一个接管。

## 构建块

- **生命周期** —— 统一信号语义(`SIGTERM` / `SIGINT` = 排空,`SIGHUP` = 重载,`SIGQUIT` = 立即),通过 `DrainController`。
- **探针** —— 分离的 `/healthz`(存活)+ `/readyz`(就绪,含排空位),让负载均衡器和编排器能路由与摘除节点。
- **Worker** —— 被监督的子进程资源,每个是一个故障隔离边界,带 OTP 风格重启策略和滑动窗口限频。
- **监听交接** —— socket activation 监听继承 + 普通 bind 回退,实现零停机重启。
- **协调锁** —— 可插拔的 `CoordinationLock` trait(`file-lock` / `pg-lock` / `lease`),用于协调并发写或选主。

## 快速开始

```toml
[dependencies]
plana = { git = "https://github.com/celestia-island/plana.git", branch = "dev" }
# 特性: socket-activation, file-lock, lease, pg-lock, replica, leader-follower
```

```rust
use plana::{acquire_listener, probe_router, ProbeState, DrainController};

#[tokio::main]
async fn main() -> std::io::Result<()> {
    // 监听交接: socket activation,回退到普通 bind。
    let listener = acquire_listener("0.0.0.0:8080").await?;

    // 探针 + 信号感知的排空。
    let probe = ProbeState::new(env!("CARGO_PKG_VERSION"));
    let ctrl = DrainController::install();

    let app = axum::Router::new()
        .merge(probe_router(probe)) // GET /healthz, GET /readyz
        .with_state(());

    axum::serve(listener, app)
        .with_graceful_shutdown(async {
            // 在 SIGINT / SIGTERM(排空)或 SIGQUIT(立即)时完成,
            // 但 SIGHUP(重载)不会 —— 服务继续运行。
            ctrl.wait_for_drain().await;
        })
        .await?;
    Ok(())
}
```

## 特性开关

| 特性 | 启用 |
| --- | --- |
| `socket-activation` | 继承监听 fd(socket activation) |
| `file-lock` | POSIX `flock` `CoordinationLock` 后端 |
| `lease` | 带租约的文件锁,崩溃自动过期 |
| `pg-lock` | PostgreSQL `pg_advisory_lock` 后端(待实现) |
| `replica` | `InstanceRegistry` trait(负载均衡 / 滚动更新) |
| `leader-follower` | `LeaderElector` trait(主动-被动 HA) |

## 状态

生命周期 + 探针、被监督的 worker、监听交接,以及带 `file-lock` 后端的协调锁 trait 已实现。`replica` / `leader-follower` 策略后端是 trait 契约,完整实现待补。

## 许可证

Business Source License 1.1 (BSL-1.1);于 2030-01-01 自动转换为你选择的 Apache-2.0 或 MIT。见 [LICENSE](../LICENSE)。
