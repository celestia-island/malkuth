# 快速开始

## 将 malkuth 添加到你的项目

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

## 带有优雅关闭和探针的最小服务器

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

## 你将获得

| 端点 | 用途 |
| --- | --- |
| `GET /healthz` | 存活探针 —— "进程还活着"（pid、运行时间、版本） |
| `GET /readyz` | 就绪探针 —— "可以服务"（排空期间返回 503） |

| 信号 | 行为 |
| --- | --- |
| `SIGINT` / `SIGTERM` | 优雅排空（完成进行中的请求，然后退出） |
| `SIGHUP` | 热重载（**不会**退出 —— 服务器继续服务） |
| `SIGQUIT` | 立即退出（仅限紧急情况） |

## 功能开关

| 功能 | 启用 |
| --- | --- |
| `socket-activation` | 从 systemd 继承监听器 fd（零停机重启） |
| `file-lock` | 基于 POSIX `flock` 的 `CoordinationLock` 后端 |
| `lease` | 崩溃时带 TTL 自动过期的基于租约的文件锁 |
| `replica` | 用于负载均衡副本的 `InstanceRegistry` trait |
| `leader-follower` | 用于主备高可用的 `LeaderElector` trait |

所有功能均为可选；默认构建不包含任何 unsafe 代码，仅依赖 tokio + axum。
