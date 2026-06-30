# 工作行程監督

## 模型

一個**工作行程**是一個可獨立終止的子行程，持有恰好一個資源（PLC 連線、序列埠、
cosmos 或 pglite-proxy 之類的 sidecar）。子行程是**故障隔離邊界**：如果資源
崩潰，只有工作行程重啟 —— 父行程繼續服務。

## 定義工作行程

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

## 重啟策略

借鑑自 Erlang/OTP：

| 策略 | 何時重啟…… |
| --- | --- |
| `Permanent`（預設） | 任何退出，即使是正常退出 |
| `Transient` | 僅異常（非零）退出 |
| `Temporary` | 永不 |

## 速率限制

監督器套用**滑動窗口速率限制**來防止崩潰風暴：

```rust
let supervisor = Supervisor::new(workers)
    .rate_limit(5, std::time::Duration::from_secs(60)) // max 5 restarts / 60s
    .cooldown(std::time::Duration::from_secs(30));      // then cooldown 30s
```

如果工作行程在窗口內崩潰次數超過 `max_restarts`，它會在下一次嘗試前進入
冷卻期。

## 執行監督器

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

## 工作行程狀態快照

`supervisor.run()` 完成（關閉時）後，回傳一個 `Vec<WorkerInfo>`，包含每個工作行程
的最終狀態、重啟次數和最後一次錯誤 —— 適用於日誌記錄或向監控系統報告。
