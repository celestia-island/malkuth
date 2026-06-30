# 우아한 종료와 드레인

## 문제점

대부분의 Rust 서버는 `ctrl_c`(SIGINT)만 잡습니다. 하지만 `docker stop`, `systemctl restart`,
그리고 Kubernetes 파드 종료는 **SIGTERM**을 보냅니다 —— 이는 우아한 종료를 우회하여
진행 중인 요청을 강제로 종료합니다.

## 해결책: `DrainController`

`DrainController::install()`은 nginx/Go 관례를 따라 표준 시그널 핸들러를 설정합니다:

| 시그널 | 의미 | 드레인 여부 |
| --- | --- | --- |
| `SIGINT` / `SIGTERM` | 우아한 종료 | 함 |
| `SIGHUP` | 핫 설정 리로드 | 안 함（서버가 계속 서비스） |
| `SIGQUIT` | 즉시 종료 | 함（드레인 건너뜀） |

## 사용법

```rust
use malkuth::DrainController;

let ctrl = DrainController::install();

// Pass clones to whoever needs to observe drain:
// - the serve loop (to stop accepting)
// - the probe layer (to set the /readyz draining bit)
// - background tasks (to wind down)

// Block until a drain/immediate signal fires.
let kind = ctrl.wait_for_drain().await;
```

## `axum::serve`에 연결하기

```rust
axum::serve(listener, app)
    .with_graceful_shutdown(async {
        ctrl.wait_for_drain().await;
    })
    .await?;
```

`wait_for_drain`은 `SIGINT`/`SIGTERM`/`SIGQUIT`에서 완료되지만 `SIGHUP`에서는 완료**되지 않습니다**.
따라서 리로드로 인해 서버가 실수로 종료되지 않습니다.

## 드레인 상태 관찰

```rust
// Non-blocking check:
if ctrl.is_draining() {
    // refuse new work
}

// Sleep, but wake early if drain begins:
ctrl.sleep_or_drain(std::time::Duration::from_secs(30)).await;
```

## 프로그래밍 방식 드레인

프로세스 내부에서 드레인을 트리거할 수도 있습니다（예: 관리 RPC）:

```rust
ctrl.begin_drain(malkuth::ShutdownKind::Graceful);
```

## `ShutdownKind`

```rust
pub enum ShutdownKind {
    Graceful,   // SIGINT / SIGTERM — drain, then exit 0
    Immediate,  // SIGQUIT — skip drain, exit fast
    Reload,     // SIGHUP — reload config, do NOT exit
}
```
