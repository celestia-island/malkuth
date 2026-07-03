# ヘルスプローブ

## `ProbeSink` トレイト

Malkuth は**プローブ状態**を**それを公開する方法**から分離します。
`ProbeSink` トレイトは 2 つのクエリを定義します：

```rust
#[async_trait]
pub trait ProbeSink: Send + Sync {
    async fn ready(&self) -> ReadyStatus;
    async fn health(&self) -> HealthStatus;
}
```

`ProbeSink` を実装する任意の型は、JSON-RPC または HTTP 経由で照会できます。

## `ProbeState` —— 組み込み実装

`ProbeState` はバージョン情報、ドレイン状態フラグ、世代カウンタ、
依存関係チェックのリストを保持します：

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

## JSON-RPC による公開（主要）

`Router::lifecycle(ctrl, Some(probe))` は標準メソッドを登録し、呼び出しごとに
`ProbeSink` を照会します：

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

`draining` が `true` のとき、あるいは任意の依存関係が `ok: false` のとき、
`ready` は `false` になります。

## HTTP による公開（オプション、feature `probes`）

HTTP を期待する Kubernetes 風 HTTP プローブや外部ロードバランサ向けに、
`probes` feature を有効化すると axum ルートを取得できます：

```rust
use malkuth::{ProbeState, probe_router};

let probe = ProbeState::new(env!("CARGO_PKG_VERSION"));
probe.add_dependency("database", || { true });

let app = axum::Router::new()
    .merge(probe_router(probe));   // GET /healthz + GET /readyz
```

| エンドポイント | 戻り値 | HTTP ステータス |
| --- | --- | --- |
| `GET /healthz` | `HealthStatus` | 常に 200 |
| `GET /readyz` | `ReadyStatus` | レディ時は 200、ドレイン中 / 依存異常時は 503 |

レスポンスの形状は JSON-RPC メソッドと同一です —— `ProbeState` は
`ProbeSink` を実装しているので、両方のパスが同じ基盤状態を照会します。

## ドレインをプローブに接続

グレースフルシャットダウン中にドレイン状態を設定し、`Lifecycle.Status`
（および `/readyz`）に反映させます：

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

これにより、オーケストレータはプロセスが終了する**前に**レディ状態が
`false` に切り替わるのを検知します —— これがダウンタイムゼロの
ローリングアップデートの中核です。
