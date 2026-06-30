# 워커 감독

## 모델

**워커**는 독립적으로 종료할 수 있는 자식 프로세스로, 정확히 하나의 자원(PLC 연결,
직렬 포트, cosmos나 pglite-proxy 같은 sidecar)을 보유합니다. 자식 프로세스는
**장애 격리 경계**입니다: 자원이 충돌하면 워커만 재시작됩니다 —— 부모는 계속 서비스합니다.

## 워커 정의

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

## 재시작 정책

Erlang/OTP에서 차용했습니다:

| 정책 | 재시작 시기… |
| --- | --- |
| `Permanent`(기본값) | 모든 종료, 정상 종료도 포함 |
| `Transient` | 비정상(0이 아닌) 종료만 |
| `Temporary` | 없음 |

## 속도 제한

수퍼바이저는 충돌 폭풍을 방지하기 위해 **슬라이딩 윈도우 속도 제한**을 적용합니다:

```rust
let supervisor = Supervisor::new(workers)
    .rate_limit(5, std::time::Duration::from_secs(60)) // max 5 restarts / 60s
    .cooldown(std::time::Duration::from_secs(30));      // then cooldown 30s
```

워커가 윈도우 내에서 `max_restarts` 횟수를 초과하여 충돌하면, 다음 시도 전에
쿨다운 기간에 진입합니다.

## 수퍼바이저 실행

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

## 워커 상태 스냅샷

`supervisor.run()`이 완료되면(종료 시), 각 워커의 최종 상태, 재시작 횟수, 마지막 에러를
포함하는 `Vec<WorkerInfo>`를 반환합니다 —— 로깅이나 모니터링 시스템에 보고하는 데 유용합니다.
