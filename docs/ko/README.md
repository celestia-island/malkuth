<!-- markdownlint-disable MD033 MD041 MD036 -->
<div align="center">

<img src="../res/logo.webp" alt="Plana logo" width="200"/>

# Plana

**장기 실행 프로그램이 스스로 업그레이드하고 부하를 분산할 수 있도록 돕는 인프라**

[![License](https://img.shields.io/badge/license-BSL--1.1-blue.svg)](../LICENSE)
[![Rust](https://img.shields.io/badge/rust-1.85%2B-orange.svg)](https://www.rust-lang.org/)
[![GitHub](https://img.shields.io/badge/github-celestia--island%2Fplana-blue.svg)](https://github.com/celestia-island/plana)

</div>
<!-- markdownlint-enable MD033 MD041 MD036 -->

**[English](../../README.md)** &bull; **[简体中文](../zhs/README.md)** &bull;
**[繁體中文](../zht/README.md)** &bull; **[日本語](../ja/README.md)** &bull;
**[한국어](../ko/README.md)** &bull; **[Français](../fr/README.md)** &bull;
**[Español](../es/README.md)** &bull; **[Русский](../ru/README.md)**

> **버전 0.1.0** — 초기 개발 단계. 독립적이고 자체 완비형이며, tokio + axum에만
> 의존합니다.

Plana는 자동화된 장기 실행 프로그램 — 데몬, 에이전트, 서버 — 가 두 가지 어려운 일을
안전하게 수행하도록 돕습니다:

- **자가 업그레이드** — 진행 중인 작업이나 연결을 잃지 않고 새 버전(또는 새로
  컴파일한 빌드)을 배포합니다: 무중단 롤링 업데이트.
- **부하 분산** — 여러 인스턴스가 작업을 나누고 상태를 조정하며, 하나는 우아하게
  물러나고 다른 하나가 인계받을 수 있습니다.

## 구성 요소

- **수명 주기** — `DrainController`를 통한 통일된 시그널 의미(`SIGTERM` / `SIGINT` =
  드레인, `SIGHUP` = 리로드, `SIGQUIT` = 즉시).
- **프로브** — `/healthz`(활성) + `/readyz`(준비, 드레인 비트 포함)를 분리하여 로드
  밸런서와 오케스트레이터가 노드를 라우팅하고 제거할 수 있게 합니다.
- **워커** — 감독되는 자식 프로세스 리소스로, 각각이 장애 격리 경계이며 OTP
  스타일의 재시작 정책과 슬라이딩 윈도우 속도 제한을 갖습니다.
- **리스너 인계** — socket-activation 리스너 상속과 일반 bind 폴백으로 무중단
  재시작을 구현합니다.
- **조정 잠금** — 동시 쓰기를 조정하거나 리더 선출에 사용하는, 플러그 가능한
  `CoordinationLock` 트레이트(`file-lock` / `pg-lock` / `lease`).

## 빠른 시작

```toml
[dependencies]
plana = { git = "https://github.com/celestia-island/plana.git", branch = "dev" }
# features: socket-activation, file-lock, lease, pg-lock, replica, leader-follower
```

```rust
use plana::{acquire_listener, probe_router, ProbeState, DrainController};

#[tokio::main]
async fn main() -> std::io::Result<()> {
    // Listener handoff: socket activation, falls back to a plain bind.
    let listener = acquire_listener("0.0.0.0:8080").await?;

    // Probes + signal-aware drain.
    let probe = ProbeState::new(env!("CARGO_PKG_VERSION"));
    let ctrl = DrainController::install();

    let app = axum::Router::new()
        .merge(probe_router(probe)) // GET /healthz, GET /readyz
        .with_state(());

    axum::serve(listener, app)
        .with_graceful_shutdown(async {
            // Resolves on SIGINT / SIGTERM (drain) or SIGQUIT (immediate),
            // but NOT on SIGHUP (reload — the server keeps serving).
            ctrl.wait_for_drain().await;
        })
        .await?;
    Ok(())
}
```

## 기능 플래그

| 기능 | 활성화 |
| --- | --- |
| `socket-activation` | 리스너 fd를 상속(socket activation) |
| `file-lock` | POSIX `flock` `CoordinationLock` 백엔드 |
| `lease` | TTL 자동 만료가 있는 임대 기반 파일 잠금 |
| `pg-lock` | PostgreSQL `pg_advisory_lock` 백엔드(예정) |
| `replica` | `InstanceRegistry` 트레이트(부하 분산 / 롤링 업데이트) |
| `leader-follower` | `LeaderElector` 트레이트(액티브-패시브 HA) |

## 상태

수명 주기 + 프로브, 감독되는 워커, 리스너 인계, 그리고 `file-lock` 백엔드가 있는
조정 잠금 트레이트가 구현되었습니다. `replica` / `leader-follower` 전략 백엔드는
트레이트 계약이며, 전체 구현이 예정되어 있습니다.

## 라이선스

Business Source License 1.1 (BSL-1.1); 2030-01-01에 자동으로 선택한 Apache-2.0
또는 MIT로 전환됩니다. [LICENSE](../LICENSE)를 참조하세요.
