# 빠른 시작

## malkuth를 프로젝트에 추가하기

```toml
[dependencies]
malkuth = { git = "https://github.com/celestia-island/malkuth.git", branch = "dev" }
# Optional features:
#   socket-activation  — inherit a listener fd from systemd
#   file-lock          — POSIX flock coordination-lock backend
#   lease              — lease-based file lock with TTL auto-expiry
#   replica            — InstanceRegistry trait (load-balancing)
#   leader-follower    — LeaderElector trait (active-passive HA)
```

## 우아한 종료와 프로브를 갖춘 최소 서버

```rust
use malkuth::{acquire_listener, probe_router, ProbeState, DrainController};

#[tokio::main]
async fn main() -> std::io::Result<()> {
    // 1. Acquire a listener — prefers systemd socket activation,
    //    falls back to binding the given address.
    let listener = acquire_listener("0.0.0.0:8080").await?;

    // 2. Create probe state and install the drain controller.
    let probe = ProbeState::new(env!("CARGO_PKG_VERSION"));
    let ctrl = DrainController::install();

    // 3. Build your router, merging the probe routes.
    let app = axum::Router::new()
        .route("/", axum::routing::get(|| async { "hello" }))
        .merge(probe_router(probe))
        .with_state(());

    // 4. Serve with graceful shutdown: SIGINT/SIGTERM trigger drain,
    //    SIGQUIT forces immediate exit, SIGHUP reloads (keeps serving).
    axum::serve(listener, app)
        .with_graceful_shutdown(async {
            ctrl.wait_for_drain().await;
        })
        .await?;
    Ok(())
}
```

## 얻을 수 있는 것

| 엔드포인트 | 용도 |
| --- | --- |
| `GET /healthz` | 라이브니스 — "프로세스가 살아 있음"（pid, 가동 시간, 버전） |
| `GET /readyz` | 레디니스 — "서비스 가능"（드레인 중에는 503 반환） |

| 시그널 | 동작 |
| --- | --- |
| `SIGINT` / `SIGTERM` | 우아한 드레인（진행 중인 요청을 완료한 후 종료） |
| `SIGHUP` | 핫 리로드（종료**하지 않음** — 서버가 계속 서비스） |
| `SIGQUIT` | 즉시 종료（긴급 시에만） |

## 기능 플래그

| 기능 | 활성화 |
| --- | --- |
| `socket-activation` | systemd에서 리스너 fd 상속（무정지 재시작） |
| `file-lock` | POSIX `flock` 기반 `CoordinationLock` 백엔드 |
| `lease` | 크래시 시 TTL 자동 만료가 있는 리스 기반 파일 잠금 |
| `replica` | 부하 분산 복제본을 위한 `InstanceRegistry` 트레이트 |
| `leader-follower` | 능동-수동 HA를 위한 `LeaderElector` 트레이트 |

모든 기능은 옵트인입니다. 기본 빌드에는 unsafe 코드가 없으며 tokio + axum에만 의존합니다.
