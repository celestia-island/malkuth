# Worker 監督

## 模型

一個 **worker** 是一個可被獨立殺死（kill）的子行程，它恰好持有一個資源
（一個 PLC 連線、一個序列埠、一個像 cosmos 或 pglite-proxy 這樣的 sidecar）。
子行程就是**故障隔離的邊界**：如果資源崩潰，只有 worker 會重啟 —— 父行程
繼續提供服務。

## 定義 worker

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

## 重啟策略

借鑑自 Erlang/OTP：

| 策略 | 在……時重啟 |
| --- | --- |
| `Permanent`（預設） | 任何退出，即使是乾淨退出 |
| `Transient` | 僅異常（非零）退出 |
| `Temporary` | 永不 |

## 速率限制

supervisor 套用一個**滑動視窗速率限制**，以防止崩潰風暴：

```rust
let supervisor = Supervisor::new(workers)
    .rate_limit(5, std::time::Duration::from_secs(60)) // max 5 restarts / 60s
    .cooldown(std::time::Duration::from_secs(30));      // then cooldown 30s
```

如果一個 worker 在視窗內崩潰次數超過 `max_restarts`，它會在下一次嘗試之前
進入冷卻期。

## 執行 supervisor

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

`Supervisor::run` 把每個子行程的退出與 `wait_for_drain()` 進行競速。在排空時，
所有子行程都會被殺死（`kill_on_drop`），並回傳最終的 `WorkerInfo` 快照。

## Worker 狀態快照

`supervisor.run()` 完成後，它會回傳一個 `Vec<WorkerInfo>`，其中包含每個 worker
的最終狀態、重啟次數和最後一次錯誤：

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
