# ワーカースーパービジョン

## モデル

**ワーカー**は、独立して終了可能な子プロセスであり、厳密に1つのリソース（PLC 接続、
シリアルポート、cosmos や pglite-proxy のような sidecar）を保持します。子プロセスは
**障害分離の境界**です：リソースがクラッシュしても、再起動するのはワーカーだけです —— 
親プロセスはサービスを継続します。

## ワーカーの定義

```rust
use malkuth::{Supervisor, WorkerSpec};
use malkuth::RestartPolicy;

let workers = vec![
    WorkerSpec::new("plc-1", "modbus", "/usr/bin/modbus-bridge")
        .args(["--device", "/dev/ttyUSB0"])
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

スーパーバイザーは、クラッシュストームを防ぐために**スライディングウィンドウ型レート制限**を
適用します：

```rust
let supervisor = Supervisor::new(workers)
    .rate_limit(5, std::time::Duration::from_secs(60)) // max 5 restarts / 60s
    .cooldown(std::time::Duration::from_secs(30));      // then cooldown 30s
```

ワーカーがウィンドウ内で `max_restarts` 回を超えてクラッシュした場合、次の試行前に
クールダウン期間に入ります。

## スーパーバイザーの実行

```rust
use tokio::sync::watch;

let (shutdown_tx, shutdown_rx) = watch::channel(false);

let supervisor = Supervisor::new(workers)
    .rate_limit(5, std::time::Duration::from_secs(60));

// Run until shutdown signal:
tokio::spawn(async move {
    let final_status = supervisor.run(shutdown_rx).await;
    for w in &final_status {
        tracing::info!(worker = %w.id, status = ?w.status, restarts = w.restart_count, "final");
    }
});

// Later, trigger shutdown:
let _ = shutdown_tx.send(true);
```

## ワーカー状態のスナップショット

`supervisor.run()` が完了（シャットダウン時）すると、各ワーカーの最終状態、再起動回数、
最後のエラーを含む `Vec<WorkerInfo>` を返します —— ログ記録や監視システムへの報告に役立ちます。
