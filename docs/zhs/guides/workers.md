# 工作进程监督

## 模型

一个**工作进程**是一个可独立终止的子进程，持有恰好一个资源（PLC 连接、串口、
cosmos 或 pglite-proxy 之类的 sidecar）。子进程是**故障隔离边界**：如果资源
崩溃，只有工作进程重启 —— 父进程继续服务。

## 定义工作进程

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

## 重启策略

借鉴自 Erlang/OTP：

| 策略 | 何时重启…… |
| --- | --- |
| `Permanent`（默认） | 任何退出，即使是正常退出 |
| `Transient` | 仅异常（非零）退出 |
| `Temporary` | 永不 |

## 速率限制

监督器应用**滑动窗口速率限制**来防止崩溃风暴：

```rust
let supervisor = Supervisor::new(workers)
    .rate_limit(5, std::time::Duration::from_secs(60)) // max 5 restarts / 60s
    .cooldown(std::time::Duration::from_secs(30));      // then cooldown 30s
```

如果工作进程在窗口内崩溃次数超过 `max_restarts`，它会在下一次尝试前进入
冷却期。

## 运行监督器

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

## 工作进程状态快照

`supervisor.run()` 完成（关闭时）后，返回一个 `Vec<WorkerInfo>`，包含每个工作进程
的最终状态、重启次数和最后一次错误 —— 适用于日志记录或向监控系统报告。
