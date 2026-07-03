# 헬스 프로브

## `ProbeSink` 트레이트

Malkuth는 **프로브 상태**를 **노출 방식**과 분리합니다. `ProbeSink` 트레이트는
두 가지 쿼리를 정의합니다:

```rust
#[async_trait]
pub trait ProbeSink: Send + Sync {
    async fn ready(&self) -> ReadyStatus;
    async fn health(&self) -> HealthStatus;
}
```

`ProbeSink`를 구현하는 모든 타입은 JSON-RPC 또는 HTTP로 조회할 수 있습니다.

## `ProbeState` —— 내장 구현

`ProbeState`는 버전 정보, 드레인 상태 플래그, 세대 카운터, 의존성 검사 목록을
들고 있습니다:

```rust
use malkuth::{ProbeState, DrainState};

let probe = ProbeState::new(env!("CARGO_PKG_VERSION"));

// Register a dependency that affects readiness.
// The closure is synchronous — keep it cheap (read an atomic, ping a cached conn).
probe.add_dependency("database", || { /* return true if healthy */ true });

// Flip the drain bit during shutdown:
probe.set_drain_state(DrainState::Draining);

// Record the deployment generation (visible in the status response):
probe.set_generation(Some(2));
```

## JSON-RPC 노출(주요 방식)

`Router::lifecycle(ctrl, Some(probe))`는 표준 메서드를 등록하고, 호출마다
`ProbeSink`를 조회합니다:

```rust
use std::sync::Arc;
use malkuth::{ProbeState, Router, Supervised};
use malkuth::transport::TcpTransport;
use malkuth::Transport;

let supervised = Supervised::new().signals();
let ctrl = supervised.drain_controller();
let probe = Arc::new(ProbeState::new("0.1.0"));

let handler = Arc::new(
    Router::new()
        .lifecycle(ctrl, Some(probe.clone()))
        .route("ping", |_| Box::pin(async { Ok(serde_json::json!("pong")) })),
);

supervised.serve_rpc(&TcpTransport, "tcp://0.0.0.0:8080", handler).await?;
```

### `Lifecycle.Health` → `HealthStatus`

```json
// Request: { "jsonrpc": "2.0", "id": 1, "method": "Lifecycle.Health", "params": {} }
// Response:
{ "alive": true, "pid": 12345, "uptime_secs": 360, "version": "0.1.0" }
```

### `Lifecycle.Status` → `ReadyStatus`

```json
// Request: { "jsonrpc": "2.0", "id": 2, "method": "Lifecycle.Status", "params": {} }
// Response:
{
  "ready": true,
  "draining": false,
  "dependencies": [{ "name": "database", "ok": true }],
  "generation": 2
}
```

`draining`이 `true`이거나 어떤 의존성이 `ok: false`이면 `ready`는 `false`가 됩니다.

## HTTP 노출(선택, feature `probes`)

HTTP를 기대하는 Kubernetes 스타일의 HTTP 프로브나 외부 로드밸런서를 위해,
`probes` feature를 활성화하면 axum 라우트를 얻을 수 있습니다:

```rust
use malkuth::{ProbeState, probe_router};

let probe = ProbeState::new(env!("CARGO_PKG_VERSION"));
probe.add_dependency("database", || { true });

let app = axum::Router::new()
    .merge(probe_router(probe));   // GET /healthz + GET /readyz
```

| 엔드포인트 | 반환 | HTTP 상태 |
| --- | --- | --- |
| `GET /healthz` | `HealthStatus` | 항상 200 |
| `GET /readyz` | `ReadyStatus` | 준비 시 200, 드레인 중 / 의존성 장애 시 503 |

응답 형태는 JSON-RPC 메서드와 동일합니다 —— `ProbeState`가 `ProbeSink`를
구현하므로 두 경로 모두 동일한 기반 상태를 조회합니다.

## 드레인을 프로브에 연결

우아한 종료 중에 드레인 상태를 설정해 `Lifecycle.Status`(그리고 `/readyz`)에
반영되게 합니다:

```rust
use malkuth::{DrainController, DrainState, ShutdownKind};

let ctrl = DrainController::new();
let probe = ProbeState::new("0.1.0");

tokio::spawn({
    let probe = probe.clone();
    let ctrl = ctrl.clone();
    async move {
        ctrl.wait_for_drain().await;
        probe.set_drain_state(DrainState::Draining);
    }
});
```

이제 오케스트레이터는 프로세스가 종료되기 **전에** 준비 비트가 `false`로
전환되는 것을 봅니다 —— 이것이 무정지 롤링 업데이트의 핵심입니다.
