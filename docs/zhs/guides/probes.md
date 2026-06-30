# 健康探针

## 拆分探针：`/healthz` 与 `/readyz`

Malkuth 遵循 Kubernetes 的惯例，提供**两个独立的探针端点**：

- **`GET /healthz`** —— *存活*："进程还活着吗？"如果失败，编排器会
  **重启**该实例。
- **`GET /readyz`** —— *就绪*："此实例现在能服务流量吗？"如果失败，
  编排器会**停止路由流量**，但不会重启。

这个区分在滚动更新期间很重要：一个正在排空的实例是*存活的*
（healthz = 200）但*未就绪*（readyz = 503）。

## 设置

```rust
use malkuth::{ProbeState, probe_router};

let probe = ProbeState::new(env!("CARGO_PKG_VERSION"));

// Register dependencies that affect readiness:
probe.add_dependency("database", || {
    // Return true if the DB connection is healthy.
    // This is a sync closure — keep it cheap (read an atomic, ping a cached conn).
    true
}).await;

// Merge the probe routes into your app:
let app = axum::Router::new()
    .merge(probe_router(probe));
```

## 响应格式

### `/healthz`（只要进程能应答就始终返回 200）

```json
{
  "alive": true,
  "pid": 12345,
  "uptime_secs": 3600,
  "version": "0.1.0"
}
```

### `/readyz`（未就绪时返回 503）

```json
{
  "ready": true,
  "draining": false,
  "dependencies": [
    { "name": "database", "ok": true }
  ],
  "generation": 2
}
```

当正在排空或某个依赖不健康时，`ready` 为 `false`，HTTP 状态码为
`503 Service Unavailable`。

## 接入排空位

在优雅关闭期间，设置排空标志，使 `/readyz` 开始返回 503：

```rust
let probe = ProbeState::new(env!("CARGO_PKG_VERSION"));
let ctrl = DrainController::install();

// In your shutdown sequence:
tokio::spawn({
    let probe = probe.clone();
    let ctrl = ctrl.clone();
    async move {
        // Wait until drain begins, then flip the bit.
        ctrl.wait_for_drain().await;
        probe.set_draining(true).await;
    }
});
```

这样负载均衡器会看到 `/readyz` 变为 503，并在进程退出**之前**停止发送
新流量 —— 这正是零停机滚动更新的核心。

## 部署世代

跟踪此实例所属的部署世代：

```rust
probe.set_generation(Some(2)).await; // generation 2 of a rolling update
```

这会包含在 `/readyz` 响应中，用于可观测性和编排。
