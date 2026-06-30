# 健康探針

## 拆分探針：`/healthz` 與 `/readyz`

Malkuth 遵循 Kubernetes 的慣例，提供**兩個獨立的探針端點**：

- **`GET /healthz`** —— *存活度*：「行程還活著嗎？」如果失敗，協調器會
  **重啟**該執行個體。
- **`GET /readyz`** —— *就緒度*：「此執行個體現在能服務流量嗎？」如果失敗，
  協調器會**停止路由流量**，但不會重啟。

這個區分在滾動更新期間很重要：一個正在排空的執行個體是*存活的*
（healthz = 200）但*未就緒*（readyz = 503）。

## 設定

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

## 回應格式

### `/healthz`（只要行程能應答就始終回傳 200）

```json
{
  "alive": true,
  "pid": 12345,
  "uptime_secs": 3600,
  "version": "0.1.0"
}
```

### `/readyz`（未就緒時回傳 503）

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

當正在排空或某個相依性不健康時，`ready` 為 `false`，HTTP 狀態碼為
`503 Service Unavailable`。

## 接入排空位元

在優雅關閉期間，設定排空旗標，使 `/readyz` 開始回傳 503：

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

這樣負載平衡器會看到 `/readyz` 變為 503，並在行程退出**之前**停止發送
新流量 —— 這正是零停機滾動更新的核心。

## 部署世代

追蹤此執行個體所屬的部署世代：

```rust
probe.set_generation(Some(2)).await; // generation 2 of a rolling update
```

這會包含在 `/readyz` 回應中，用於可觀測性與協調。
