# 헬스 프로브

## 분할 프로브: `/healthz`와 `/readyz`

Malkuth는 Kubernetes 관례를 따라 **두 개의 개별 프로브 엔드포인트**를 제공합니다:

- **`GET /healthz`** —— *라이브니스*: "프로세스가 살아 있는가?" 이것이 실패하면
  오케스트레이터는 해당 인스턴스를**재시작**합니다.
- **`GET /readyz`** —— *레디니스*: "이 인스턴스가 지금 트래픽을 처리할 수 있는가?"
  이것이 실패하면 오케스트레이터는**트래픽 라우팅을 중지**하지만 재시작하지는 않습니다.

이 구분은 롤링 업데이트 중에 중요합니다: 드레인 중인 인스턴스는
*살아 있습니다*（healthz = 200）하지만 *준비되지 않았습니다*（readyz = 503）.

## 설정

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

## 응답 형식

### `/healthz`（프로세스가 응답할 수 있는 한 항상 200）

```json
{
  "alive": true,
  "pid": 12345,
  "uptime_secs": 3600,
  "version": "0.1.0"
}
```

### `/readyz`（준비되지 않았을 때 503）

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

드레인 중이거나 의존성이 비정상일 때, `ready`는 `false`가 되며 HTTP 상태 코드는
`503 Service Unavailable`이 됩니다.

## 드레인 비트 연결

우아한 종료 중에 드레인 플래그를 설정하여 `/readyz`가 503을 반환하도록 합니다:

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

이렇게 하면 로드 밸런서가 `/readyz`가 503이 되는 것을 감지하고, 프로세스가
종료되기 **전에** 새 트래픽 전송을 중지합니다 —— 이것이 무정지 롤링 업데이트의 핵심입니다.

## 배포 세대

이 인스턴스가 속한 배포 세대를 추적합니다:

```rust
probe.set_generation(Some(2)).await; // generation 2 of a rolling update
```

이 값은 가시성과 오케스트레이션을 위해 `/readyz` 응답에 포함됩니다.
