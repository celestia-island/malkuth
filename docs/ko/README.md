<p align="center"><img src="https://raw.githubusercontent.com/celestia-island/malkuth/master/docs/logo.webp" alt="Malkuth" width="240" /></p>

<h1 align="center">Malkuth</h1>

<p align="center"><strong>Rust용 컴포저블 서비스 감독 툴킷</strong></p>

<div align="center">

[![License: SySL-1.0](https://img.shields.io/badge/License-SySL--1.0-blue.svg)](https://sysl.celestia.world)
[![GitHub](https://img.shields.io/badge/github-celestia--island%2Fmalkuth-blue.svg)](https://github.com/celestia-island/malkuth)
[![Checks](https://img.shields.io/github/actions/workflow/status/celestia-island/malkuth/checks.yml)](https://github.com/celestia-island/malkuth/actions/workflows/checks.yml)
[![Docs](https://img.shields.io/badge/docs-malkuth.docs.celestia.world-blue)](https://malkuth.docs.celestia.world)
[![docs.rs](https://docs.rs/malkuth/badge.svg)](https://docs.rs/malkuth)

</div>

<div align="center">

[English](../en/README.md) · [简体中文](../zhs/README.md) · [繁體中文](../zht/README.md) · [日本語](../ja/README.md) · **한국어** · [Français](../fr/README.md) · [Español](../es/README.md) · [Русский](../ru/README.md) · [العربية](../ar/README.md)

</div>

Malkuth는 자동화되어 장시간 실행되는 프로그램이 네 가지 어려운 일을 해결하도록 돕습니다.

1. **플러그 가능한 트랜스포트** — 로컬 TCP 루프백, 원격
   **WebSocket** 또는 로컬 **IPC**([`interprocess`](https://crates.io/crates/interprocess) 기반
   유닉스 소켓 / 명명된 파이프) 위의 JSON-RPC. 단일 `Transport`
   트레이트를 URL 스킴으로 디스패치합니다.
2. **감독되는 워커** — 프로세스를 생성하고, 상태를 모니터링하며, 실패 시 재시작하고, 종료 전에 연결을 드레인합니다.
3. **선택적이고 훅 가능한 기능** — 종료 소스, 프로브, 하트비트와 드레인
   훅은 *트레이트*입니다. 기본값(OS 시그널 종료, axum 프로브, 감독되는
   워커)을 사용하거나 직접 제공하세요(예: 서버가 수신한 대역 내 "stop" 명령으로
   드레인을 트리거). 배터리 포함 `Supervised` 오케스트레이터가 이들을
   연결해 줍니다.
4. **watchdog CLI** — `malkuth -- <cmd>`는 프로그램을 파일 감시,
   pod 풀, L4 스티키 리버스 프록시로 감쌉니다.

## CLI로 사용

```
malkuth [--watch PATH]... [--proxy PUBLIC:LO-HI] [--pod-count N] -- <cmd> [args...]
```

서버의 병렬 복사본 5개를 실행하고(각각 `PORT` 환경 변수로 수신 →
3001–3005 자동 할당), 3000번 포트의 스티키 프록시가 앞단에 위치합니다:

```bash
malkuth --watch ./src --watch ./res \
        --proxy 3000:3000-3999 --pod-count 5 \
        -- cargo run
```

프록시는 각 **클라이언트 IP**를 일관된 해싱으로 고정된 백엔드로 라우팅하므로,
클라이언트는 pod가 재시작되거나 축소될 때까지 동일한 pod를 계속 사용합니다 —
이것이 그레이 릴리스 / 롤링 재시작의 기반입니다. 파일 변경 시 한 번에 하나의
pod씩 드레인 및 재시작합니다.

## 라이브러리로 사용

```toml
[dependencies]
malkuth = "0.1"
# features: tcp (default) | ws | ipc | signals (default) | worker | probes |
#           file-lock | lease | pg-lock | replica | leader-follower | schema | cli
```

```rust
use std::sync::Arc;
use malkuth::{Client, Router, Server, Supervised, Transport};
use malkuth::transport::TcpTransport;
use serde_json::json;

#[tokio::main]
async fn main() -> std::io::Result<()> {
    // Bind once; build a router with the standard lifecycle RPC + your methods.
    let lis = TcpTransport.listen("tcp://127.0.0.1:0").await?;
    let supervised = Supervised::new().signals();           // OS-signal exit source
    let ctrl = supervised.drain_controller();
    let handler = Arc::new(
        Router::new()
            .lifecycle(ctrl, None)                          // Lifecycle.Drain/Status/...
            .route("ping", |_| Box::pin(async { Ok(json!("pong")) })),
    );
    // Race the server against the exit source, then run drain hooks.
    supervised.serve_rpc_listener(lis, handler).await
}
```

시그널 대신 자체 로직으로 드레인을 트리거해야 한다면?
`malkuth::ExitSource`를 구현하여 `.exit(...).`로 전달하세요. Postgres 기반
조정이 필요하다면? `pg-lock` 기능이 `CoordinationLock` 백엔드를 제공합니다.

## 기능 플래그

| 기능 | 활성화 내용 |
| --- | --- |
| `tcp` *(기본값)* | 로컬/원격 TCP 위의 JSON-RPC (`tokio::net`) |
| `ws` | WebSocket 위의 JSON-RPC (`tokio-tungstenite`) |
| `ipc` | 로컬 IPC 위의 JSON-RPC (`interprocess`) |
| `signals` *(기본값)* | 기본 OS 시그널 `ExitSource` (`tokio::signal`) |
| `worker` | 감독되는 자식 프로세스 워커 (`tokio::process`) |
| `probes` | axum `/healthz` + `/readyz` 라우터 |
| `file-lock` | POSIX `flock` `CoordinationLock` 백엔드 (unix) |
| `lease` | TTL 자동 만료가 있는 파일 임대 `CoordinationLock` (크래시 안전) |
| `pg-lock` | PostgreSQL `pg_advisory_lock` 백엔드 (`tokio-postgres`) |
| `replica` | 인메모리 `InstanceRegistry` |
| `leader-follower` | `LeaseLeaderElector` (임대 백엔드 위에서) |
| `schema` | 와이어 타입에 대한 `schemars::JsonSchema` derive |
| `cli` | `malkuth` watchdog 바이너리 (pod 풀 + 스티키 프록시) |

## 상태

레이어 1–3(라이프사이클/드레인, 프로브, 리스너 핸드오프) 및 JSON-RPC 코어
(코덱 + 서버/클라이언트 + tcp/ws/ipc 트랜스포트)가 구현되어 엔드투엔드
테스트를 통과했습니다. CLI pod 풀 + 스티키 프록시가 작동합니다(e2e 검증 완료).
세 가지 `CoordinationLock` 백엔드(`file-lock`, `lease`, `pg-lock`)와
`leader-follower` `LeaseLeaderElector`가 구현되었습니다. 설계는
[docs/design/](../en/design/)을 참조하세요.

## MCP 서버

`mcp` feature로 malkuth를 빌드하고 stdio 서버를 실행합니다——모델 컨텍스트 프로토콜(Model Context Protocol)을 통해 감독 툴킷을 AI 코딩 어시스턴트에 노출합니다:

```bash
malkuth mcp
```

서버는 두 가지 도구를 제공합니다: `malkuth_supervise`(재시작 정책 + 슬라이딩 윈도우 속도 제한 하에 감독자 아래에 worker 집합을 시작하고, 그들이 종료되거나 타임아웃이 발생할 때까지 블록한 후 최종 상태 스냅샷을 반환) 및 `malkuth_probe`(서비스 URL에 대한 HTTP healthz / readyz 검사). MCP 클라이언트에 연결하려면:

```json
{
  "mcpServers": {
    "malkuth": { "command": "malkuth", "args": ["mcp"] }
  }
}
```

`mcp` feature는 `worker` + `schema`를 암시하며, 프로브 도구용 `rmcp`와 `reqwest` 클라이언트를 추가합니다.

## 라이선스

SySL-1.0（Synthetic Source License）。[LICENSE](https://sysl.celestia.world)를 참조하세요。
