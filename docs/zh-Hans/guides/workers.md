# Worker 监督

## 模型

一个 **worker** 是一个可被独立杀死（kill）的子进程，它恰好持有一个资源
（一个 PLC 连接、一个串口、一个像 cosmos 或 pglite-proxy 这样的 sidecar）。
子进程就是**故障隔离的边界**：如果资源崩溃，只有 worker 会重启 —— 父进程
继续提供服务。

## 定义 worker

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

## 重启策略

借鉴自 Erlang/OTP：

| 策略 | 在……时重启 |
| --- | --- |
| `Permanent`（默认） | 任何退出，即使是干净退出 |
| `Transient` | 仅异常（非零）退出 |
| `Temporary` | 永不 |

## 速率限制

supervisor 应用一个**滑动窗口速率限制**，以防止崩溃风暴：

```rust
let supervisor = Supervisor::new(workers)
    .rate_limit(5, std::time::Duration::from_secs(60)) // max 5 restarts / 60s
    .cooldown(std::time::Duration::from_secs(30));      // then cooldown 30s
```

如果一个 worker 在窗口内崩溃次数超过 `max_restarts`，它会在下一次尝试之前
进入冷却期。

## 运行 supervisor

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

`Supervisor::run` 把每个子进程的退出与 `wait_for_drain()` 进行竞速。在排空时，
所有子进程都会被杀死（`kill_on_drop`），并返回最终的 `WorkerInfo` 快照。

## Worker 状态快照

`supervisor.run()` 完成后，它会返回一个 `Vec<WorkerInfo>`，其中包含每个 worker
的最终状态、重启次数和最后一次错误：

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
