# 워커 슈퍼비전

## 모델

**워커**는 독립적으로 종료(kill) 가능한 자식 프로세스로, 정확히 하나의 리소스
(PLC 연결, 직렬 포트, cosmos나 pglite-proxy 같은 sidecar)를 보유합니다.
자식 프로세스가 **장애 격리 경계**입니다: 리소스가 크래시되어도 재시작하는 것은
워커뿐입니다 —— 부모는 서비스를 계속합니다.

## 워커 정의

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

## 재시작 정책

Erlang/OTP에서 차용했습니다:

| 정책 | 재시작 시점… |
| --- | --- |
| `Permanent`(기본값) | 모든 종료, 깔끔한 종료도 포함 |
| `Transient` | 비정상(0이 아닌) 종료만 |
| `Temporary` | 없음 |

## 레이트 리밋

슈퍼바이저는 크래시 폭풍을 막기 위해 **슬라이딩 윈도우 레이트 리밋**을
적용합니다:

```rust
let supervisor = Supervisor::new(workers)
    .rate_limit(5, std::time::Duration::from_secs(60)) // max 5 restarts / 60s
    .cooldown(std::time::Duration::from_secs(30));      // then cooldown 30s
```

워커가 윈도우 내에서 `max_restarts` 횟수를 초과해 크래시하면, 다음 시도 전에
쿨다운 기간에 진입합니다.

## 슈퍼바이저 실행

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

`Supervisor::run`은 각 자식의 종료를 `wait_for_drain()`과 경쟁시킵니다.
드레인 시 모든 자식이 종료되고(`kill_on_drop`), 최종 `WorkerInfo` 스냅샷이
반환됩니다.

## 워커 상태 스냅샷

`supervisor.run()`이 완료되면, 각 워커의 최종 상태, 재시작 횟수, 마지막 에러를
담은 `Vec<WorkerInfo>`를 반환합니다:

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
