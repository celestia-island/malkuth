# 健康探針

## `ProbeSink` trait

Malkuth 將**探針狀態**與**它的暴露方式**分離開來。`ProbeSink` trait 定義了
兩個查詢：

```rust
#[async_trait]
pub trait ProbeSink: Send + Sync {
    async fn ready(&self) -> ReadyStatus;
    async fn health(&self) -> HealthStatus;
}
```

任何實作了 `ProbeSink` 的型別都可以透過 JSON-RPC 或 HTTP 被查詢。

## `ProbeState` —— 內建實作

`ProbeState` 持有版本資訊、一個排空狀態旗標、一個 generation 計數器，以及
一組相依檢查：

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

## 透過 JSON-RPC 暴露（主要方式）

`Router::lifecycle(ctrl, Some(probe))` 註冊標準方法，在每次呼叫時查詢
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

當 `draining` 為 `true` 或任意相依項為 `ok: false` 時，`ready` 為 `false`。

## 透過 HTTP 暴露（可選，feature `probes`）

對於期望 HTTP 的 Kubernetes 風格 HTTP 探針或外部負載平衡器，啟用 `probes`
feature 即可取得 axum 路由：

```rust
use malkuth::{ProbeState, probe_router};

let probe = ProbeState::new(env!("CARGO_PKG_VERSION"));
probe.add_dependency("database", || { true });

let app = axum::Router::new()
    .merge(probe_router(probe));   // GET /healthz + GET /readyz
```

| 端點 | 回傳 | HTTP 狀態 |
| --- | --- | --- |
| `GET /healthz` | `HealthStatus` | 永遠 200 |
| `GET /readyz` | `ReadyStatus` | 就緒為 200，排空中 / 相依異常為 503 |

回應結構與 JSON-RPC 方法完全一致 —— `ProbeState` 實作了 `ProbeSink`，因此
兩條路徑查詢的是同一份底層狀態。

## 把排空接入探針

在優雅關閉期間設定排空狀態，以便 `Lifecycle.Status`（以及 `/readyz`）反映它：

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

這樣編排器就會在行程退出**之前**看到就緒位元翻轉為 `false` —— 這正是零停機
滾動更新的核心所在。
