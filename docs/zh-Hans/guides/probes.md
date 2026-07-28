# 健康探针

## `ProbeSink` trait

Malkuth 将**探针状态**与**它的暴露方式**分离开来。`ProbeSink` trait 定义了
两个查询：

```rust
#[async_trait]
pub trait ProbeSink: Send + Sync {
    async fn ready(&self) -> ReadyStatus;
    async fn health(&self) -> HealthStatus;
}
```

任何实现了 `ProbeSink` 的类型都可以通过 JSON-RPC 或 HTTP 被查询。

## `ProbeState` —— 内置实现

`ProbeState` 持有版本信息、一个排空状态标志位、一个 generation 计数器，以及
一组依赖检查：

```rust
use malkuth::{ProbeState, DrainState};

let probe = ProbeState::new(env!("CARGO_PKG_VERSION"));

// Register a dependency that affects readiness.
// The closure is synchronous — keep it cheap (read an atomic, ping a cached conn).
probe.add_dependency("database", || { /* return true if healthy */ true });

// Flip the drain bit during shutdown:
probe.set_drain_state(DrainState::Draining);

// Record the deployment generation (visible in the status response):
probe.set_generation(Some(2));
```

## 通过 JSON-RPC 暴露（主要方式）

`Router::lifecycle(ctrl, Some(probe))` 注册标准方法，在每次调用时查询
`ProbeSink`：

```rust
use std::sync::Arc;
use malkuth::{ProbeState, Router, Supervised};
use malkuth::transport::TcpTransport;
use malkuth::Transport;

let supervised = Supervised::new().signals();
let ctrl = supervised.drain_controller();
let probe = Arc::new(ProbeState::new("0.1.0"));

let handler = Arc::new(
    Router::new()
        .lifecycle(ctrl, Some(probe.clone()))
        .route("ping", |_| Box::pin(async { Ok(serde_json::json!("pong")) })),
);

supervised.serve_rpc(&TcpTransport, "tcp://0.0.0.0:8080", handler).await?;
```

### `Lifecycle.Health` → `HealthStatus`

```json
// Request: { "jsonrpc": "2.0", "id": 1, "method": "Lifecycle.Health", "params": {} }
// Response:
{ "alive": true, "pid": 12345, "uptime_secs": 360, "version": "0.1.0" }
```

### `Lifecycle.Status` → `ReadyStatus`

```json
// Request: { "jsonrpc": "2.0", "id": 2, "method": "Lifecycle.Status", "params": {} }
// Response:
{
  "ready": true,
  "draining": false,
  "dependencies": [{ "name": "database", "ok": true }],
  "generation": 2
}
```

当 `draining` 为 `true` 或任意依赖项为 `ok: false` 时，`ready` 为 `false`。

## 通过 HTTP 暴露（可选，feature `probes`）

对于期望 HTTP 的 Kubernetes 风格 HTTP 探针或外部负载均衡器，启用 `probes`
feature 即可获得 axum 路由：

```rust
use malkuth::{ProbeState, probe_router};

let probe = ProbeState::new(env!("CARGO_PKG_VERSION"));
probe.add_dependency("database", || { true });

let app = axum::Router::new()
    .merge(probe_router(probe));   // GET /healthz + GET /readyz
```

| 端点 | 返回 | HTTP 状态 |
| --- | --- | --- |
| `GET /healthz` | `HealthStatus` | 始终 200 |
| `GET /readyz` | `ReadyStatus` | 就绪为 200，排空中 / 依赖异常为 503 |

响应结构与 JSON-RPC 方法完全一致 —— `ProbeState` 实现了 `ProbeSink`，因此
两条路径查询的是同一份底层状态。

## 把排空接入探针

在优雅关闭期间设置排空状态，以便 `Lifecycle.Status`（以及 `/readyz`）反映它：

```rust
use malkuth::{DrainController, DrainState, ShutdownKind};

let ctrl = DrainController::new();
let probe = ProbeState::new("0.1.0");

tokio::spawn({
    let probe = probe.clone();
    let ctrl = ctrl.clone();
    async move {
        ctrl.wait_for_drain().await;
        probe.set_drain_state(DrainState::Draining);
    }
});
```

这样编排器就会在进程退出**之前**看到就绪位翻转为 `false` —— 这正是零停机滚动
更新的核心所在。
