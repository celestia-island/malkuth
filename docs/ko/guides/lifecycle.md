# 우아한 종료와 드레인

## 문제점

대부분의 Rust 서버는 `ctrl_c`(SIGINT)만 잡습니다. 하지만 `docker stop`,
`systemctl restart`, Kubernetes pod 종료는 **SIGTERM**을 보냅니다 —— 이는
우아한 종료를 우회해 유예 기간 후 진행 중인 요청을 강제로 종료합니다.

## `DrainController`

`DrainController`는 공유 드레인 플래그를 들고 있으며, 어떤 태스크든 그것을
기다릴 수 있게 합니다. `tokio::sync::Notify` + 원자 연산 위에 구축되어 있습니다.

```rust
use malkuth::{DrainController, ShutdownKind};

let ctrl = DrainController::new();
```

## 시그널 의미론

`SignalExitSource`(feature `signals`)는 정규 시그널 핸들러를 설치합니다:

| 시그널 | `ShutdownKind` | 드레인? | 종료? |
| --- | --- | --- | --- |
| `SIGINT` / `SIGTERM` | `Graceful` | 예 | 예 |
| `SIGHUP` | `Reload` | 아니오 | 아니오(서비스 계속) |
| `SIGQUIT` | `Immediate` | 예(드레인 건너뜀) | 예 |

`SIGHUP`은 드레인을 트리거하지 **않습니다** —— 리로드 시 `wait_for_drain()`은
해결되지 않습니다. 리로드도 관찰해야 한다면 `wait_for_signal()`을 사용하세요.

## 프로그래밍 방식의 드레인

프로세스 내부에서(예: `Lifecycle.Drain` RPC에서) 드레인을 트리거합니다:

```rust
ctrl.begin_drain(ShutdownKind::Graceful); // → all wait_for_drain() callers wake
```

## 드레인 상태 관찰

```rust
// Non-blocking check:
if ctrl.is_draining() {
    // refuse new work
}

// Which kind fired?
if let Some(kind) = ctrl.kind() {
    println!("shutdown kind: {kind:?}");
}

// Async wait (resolves on Graceful or Immediate, NOT on Reload):
let kind = ctrl.wait_for_drain().await;

// Async wait for any signal (including Reload):
let kind = ctrl.wait_for_signal().await;
```

## `Supervised` 사용

`Supervised`는 `DrainController` + 종료 소스 + 드레인 훅을 하나의 serve
루프로 구성합니다:

```rust
use malkuth::{Supervised, Router};
use malkuth::transport::TcpTransport;
use malkuth::Transport;
use std::sync::Arc;

let supervised = Supervised::new()
    .signals()                          // install OS signal handler
    .on_drain(MyDrainHook)              // run cleanup during shutdown
    .drain_budget(std::time::Duration::from_secs(30));

let ctrl = supervised.drain_controller();
let handler = Arc::new(
    Router::new().lifecycle(ctrl, None)
);

// Serve JSON-RPC until a signal fires, then run drain hooks:
supervised
    .serve_rpc(&TcpTransport, "tcp://0.0.0.0:8080", handler)
    .await?;
```

## `ShutdownKind`

```rust
pub enum ShutdownKind {
    Graceful,   // SIGINT / SIGTERM — drain, then exit 0
    Immediate,  // SIGQUIT — skip drain, exit fast
    Reload,     // SIGHUP — reload config, do NOT exit
}
```
