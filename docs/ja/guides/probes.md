# ヘルスプローブ

## プローブの分離：`/healthz` と `/readyz`

Malkuth は Kubernetes の慣例に従い、**2 つの独立したプローブエンドポイント**を提供します：

- **`GET /healthz`** —— *ライブネス*：「プロセスは生きているか？」これが失敗すると、
  オーケストレータはそのインスタンスを**再起動**します。
- **`GET /readyz`** —— *レディネス*：「このインスタンスは今すぐトラフィックを処理できるか？」
  これが失敗すると、オーケストレータは**トラフィックのルーティングを停止**しますが、再起動はしません。

この区別はローリングアップデート中に重要です：ドレイン中のインスタンスは
*生きています*（healthz = 200）が、*準備ができていません*（readyz = 503）。

## セットアップ

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

## レスポンス形式

### `/healthz`（プロセスが応答できる限り常に 200）

```json
{
  "alive": true,
  "pid": 12345,
  "uptime_secs": 3600,
  "version": "0.1.0"
}
```

### `/readyz`（準備未完了時は 503）

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

ドレイン中や依存関係が異常な場合、`ready` は `false` となり、HTTP ステータスは
`503 Service Unavailable` になります。

## ドレインビットの組み込み

グレースフルシャットダウン中にドレインフラグを設定すると、`/readyz` が 503 を返すようになります：

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

これにより、ロードバランサは `/readyz` が 503 になるのを検知し、プロセスが
終了する**前に**新しいトラフィックの送信を停止します —— これがダウンタイムゼロの
ローリングアップデートの中核です。

## デプロイ世代

このインスタンスが属するデプロイ世代を追跡します：

```rust
probe.set_generation(Some(2)).await; // generation 2 of a rolling update
```

これは可観測性とオーケストレーションのために `/readyz` レスポンスに含まれます。
