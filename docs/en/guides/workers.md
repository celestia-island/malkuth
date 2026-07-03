# Worker Supervision

## The model

A **worker** is an independently-killable child process that holds exactly one
resource (a PLC connection, a serial port, a sidecar like cosmos or
pglite-proxy). The child process is the **failure-isolation boundary**: if the
resource crashes, only the worker restarts — the parent keeps serving.

## Defining workers

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

## Restart policies

Borrowed from Erlang/OTP:

| Policy | Restarts on… |
| --- | --- |
| `Permanent` (default) | Any exit, even a clean one |
| `Transient` | Abnormal (non-zero) exit only |
| `Temporary` | Never |

## Rate limiting

The supervisor applies a **sliding-window rate limit** to prevent crash storms:

```rust
let supervisor = Supervisor::new(workers)
    .rate_limit(5, std::time::Duration::from_secs(60)) // max 5 restarts / 60s
    .cooldown(std::time::Duration::from_secs(30));      // then cooldown 30s
```

If a worker crashes more than `max_restarts` times within the window, it enters
cooldown before the next attempt.

## Running the supervisor

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

`Supervisor::run` races each child's exit against `wait_for_drain()`. On drain,
all children are killed (`kill_on_drop`) and final `WorkerInfo` snapshots are
returned.

## Worker status snapshots

After `supervisor.run()` completes, it returns a `Vec<WorkerInfo>` with each
worker's final state, restart count, and last error:

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
