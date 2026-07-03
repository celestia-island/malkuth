# ワーカースーパービジョン

## モデル

**ワーカー**は、独立して終了可能な子プロセスであり、厳密に 1 つのリソース
（PLC 接続、シリアルポート、cosmos や pglite-proxy のような sidecar）を保持します。
子プロセスは**障害分離の境界**です：リソースがクラッシュしても、再起動するのは
ワーカーだけです —— 親プロセスはサービスを継続します。

## ワーカーの定義

```rust
use malkuth::{Supervisor, WorkerSpec, RestartPolicy, DrainController};

let workers = vec![
    WorkerSpec::new("plc-1", "modbus", "/usr/bin/modbus-bridge")
        .args(["--device", "/dev/ttyUSB0"])
        .env("LOG_LEVEL", "debug")
        .policy(RestartPolicy::Permanent),

    WorkerSpec::new("cosmos", "cosmos", "/usr/bin/cosmos-agent")
        .policy(RestartPolicy::Transient), // restart only on abnormal exit
];
```

## 再起動ポリシー

Erlang/OTP から借用しています：

| ポリシー | 再起動のタイミング… |
| --- | --- |
| `Permanent`（デフォルト） | あらゆる終了、クリーンな終了も含む |
| `Transient` | 異常（非ゼロ）終了のみ |
| `Temporary` | なし |

## レート制限

スーパーバイザーは、クラッシュストームを防ぐために**スライディングウィンドウ型
レート制限**を適用します：

```rust
let supervisor = Supervisor::new(workers)
    .rate_limit(5, std::time::Duration::from_secs(60)) // max 5 restarts / 60s
    .cooldown(std::time::Duration::from_secs(30));      // then cooldown 30s
```

ワーカーがウィンドウ内で `max_restarts` 回を超えてクラッシュした場合、次の試行前に
クールダウン期間に入ります。

## スーパーバイザーの実行

```rust
use malkuth::DrainController;

let drain = DrainController::new();
let supervisor = Supervisor::new(workers)
    .rate_limit(5, std::time::Duration::from_secs(60));

// Run until drain signal:
tokio::spawn(async move {
    let final_status = supervisor.run(drain).await;
    for w in &final_status {
        tracing::info!(worker = %w.id, status = ?w.status, restarts = w.restart_count, "final");
    }
});

// Later, trigger shutdown:
// drain.begin_drain(ShutdownKind::Graceful);
```

`Supervisor::run` は各子プロセスの終了を `wait_for_drain()` と競争させます。
ドレイン時に、すべての子プロセスが強制終了され（`kill_on_drop`）、最終的な
`WorkerInfo` スナップショットが返されます。

## ワーカー状態のスナップショット

`supervisor.run()` が完了すると、各ワーカーの最終状態、再起動回数、
最後のエラーを含む `Vec<WorkerInfo>` を返します：

```rust
pub struct WorkerInfo {
    pub id: String,
    pub kind: String,
    pub status: WorkerStatus,     // Starting | Running | Stopped | Failed
    pub restart_policy: RestartPolicy,
    pub restart_count: u32,
    pub last_error: Option<String>,
}
```
