# 통합 감독, 롤링 업데이트 및 복제 아키텍처
> **범위.** 본 문서는 *플랫폼 수준* 설계입니다. `core`(entelecheia /
> scepter), `webui`(shittim-chest / chest), `router`(evernight)를 가로지릅니다.
> 프로젝트별 아키텍처 문서는 각자의 `core/`, `webui/`, `router/` 하위
> 카테고리에 있으며, 본 문서는 세 프로젝트가 공동으로 소비하는 공유
> 라이프사이클 / 감독 계층을 정의합니다.
## 1. 배경 및 목표
세 프로젝트는 구조적으로 고도로 균질합니다 — 모두 **Rust(edition 2024,
MSRV 1.85) + axum 0.8 + tokio + Unix 소켓 / WebSocket 기반 JSON-RPC**이며,
이미 `arona` crate을 프로토콜 계층으로 공유하고 있습니다. 바로 이 균질성이
감독 메커니즘을 *한 번* 구축해 *세 번* 재사용할 가치가 있게 만듭니다.
이 메커니즘은 하나의 일관된 기능으로 표현되는 네 가지 중첩된 요구를
충족해야 합니다:
1. **로드 밸런싱** — 동일한 프로그램의 여러 인스턴스를 동시에 실행하여
   작업을 분담하고 IPC로 조정하면서, 동시에 데이터베이스 / 설정 / 런타임
   상태를 공유합니다.
2. **조정된 쓰기** — 어느 한 인스턴스가 공유 파일에 쓰기 직전에 나머지에게
   알리고 잠금을 획득해야, 동시 변경이 상태를 손상시키지 않습니다.
3. **롤링 업데이트** — 새 공식 릴리스(또는 갓 컴파일한 로컬 디버그 서버)가
   도착하면, 새 바이너리와 구 바이너리가 공존할 수 있습니다. 구 프로세스는
   진행 중인 작업을 마무리한 뒤 종료하고 실행 집합을 새 프로세스에 넘깁니다.
4. **엣지 내결함성** — 기기(특히 evernight 게이트웨이)에서 두 프로세스가
   leader/follower로 실행되어, 한쪽의 크래시가 기기 전체를 다운시키지
   않습니다.
### 1.1 현재 상태(본 설계가 메우는 격차)
코드 감사 결과 **세 프로젝트 모두**에서 동일한 세 가지 결함이 발견되었습니다:
| 능력 | entelecheia(scepter) | shittim-chest(chest) | evernight |
|---|---|---|---|
| 시그널 처리 | `ctrl_c` 전용(`shutdown.rs:17`) | `ctrl_c` 전용(`api.rs:465`) | `ctrl_c` 전용(`api/mod.rs:109`) |
| 드레인 로직 | HTTP만 드레인, WS / 백그라운드 태스크는 제외 | 동일 | 없음 |
| 리스너 fd 전달 | 없음 | 없음 | 없음 |
| drain 비트가 있는 `/readyz` | 없음 | `/api/health`는 있으나 drain 비트 없음 | 없음 |
가장 치명적인 문제: `SIGINT`만 잡기 때문에, `docker stop` /
`systemctl restart` — 즉 **`SIGTERM`**을 보내는 명령 — 이 정상 종료 경로를
완전히 우회하여 유예 기간 후 하드 킬합니다. 이것만 고쳐도 효과가 가장 큰
단일 변경입니다.
### 1.2 재사용할 기존 자산
- `entelecheia/packages/cli/src/evernight_daemon.rs` — 현존하는 가장 완전한
  자가 재시작 청사진: PID 잠금 파일 + 자기 reexec + 준비 대기 +
  `SIGTERM`→`SIGKILL` 폴백.
- `entelecheia/packages/scepter/src/daemon/health_daemon.rs` — 컨테이너
  롤링 업데이트를 수행하는 **JSON 매니페스트 파일 큐**(언어 독립적인 업데이트
  원시 타입).
- `entelecheia/packages/shared/infra_jsonrpc` — Unix 소켓 JSON-RPC 전송 계층.
- `shittim-chest/packages/core/src/proxy/upstream_pool.rs` — 지수 백오프
  재연결을 갖춘 다중 엔드포인트 레지스트리; 로드 밸런스 클라이언트의 바로
  쓸 수 있는 템플릿.
- `evernight/src/model_server.rs` — 배포→헬스 대기→구 버전 정지를 포함한
  `Running/Starting/Stopped/Failed` 자원 라이프사이클 상태 머신; 저장소 내
  애플리케이션 계층 롤링 업데이트의 템플릿.
## 2. 이론적 기반
이 메커니즘은 단일 이론이 아니라 각기 표준 구현을 가진, 잘 확립된 산업
패턴들의 조합입니다. 설계가 그 입증된 시맨틱을 직접 상속받도록 명시적으로
이름을 붙입니다:
| 표현된 요구 | 산업 용어 | 표준 구현 |
|---|---|---|
| 신·구 프로세스 공존; 구 프로세스는 진행 중인 작업을 끝내고 종료 | **정상 종료(graceful shutdown) / 드레인(drain)** + **롤링 업데이트** | Kubernetes Deployment, nginx / unicorn |
| "레드/블루 마커"(논의 중에 회상된 이름) | **블루-그린 배포**(두 개의 병렬 환경, 트래픽 포인터 전환). 기술된 점진적 감쇠 동작은 블루-그린보다 **롤링 업데이트 + 드레인**에 더 가깝습니다. | |
| 새 프로세스가 연결을 끊지 않고 같은 포트를 인계 | **socket activation / fd 상속 / `SO_REUSEPORT`** | systemd, nginx `USR2`, envoy hot restart |
| "쓰기 전에 상대에게 알리고 잠금" | **권고 잠금(advisory lock, `flock`/`fcntl`)/ DB 행 잠금 / 리스(lease)** | POSIX 권고 잠금, `pg_advisory_lock` |
| 프로세스 자가 치유 | **감독 트리(OTP)/ systemd / kubelet** | Erlang/OTP, systemd, s6, immortal |
| leader/follower로 크래시가 서비스를 죽이지 않게 함 | **리스 기반 leader 선출 + 펜싱(fencing)** | Chubby 리스, Raft leader 선출(서브셋), keepalived/VRRP, Pacemaker |
| 하나의 프로세스가 보유한 자원, 크래시 시 재시작 | **"let it crash" + 감독자 재시작** | Erlang/OTP 감독 트리, systemd `Restart=always` |
**롤링 업데이트의 산업 표준 레시피**(그대로 복사할 가치 있음)는
Kubernetes의 것입니다:
`maxSurge + maxUnavailable + readinessProbe + preStop hook + SIGTERM
정상 종료 + 유예 기간 + PodDisruptionBudget`. 우리가 구축하는 것은 그
레시피의 자체 호스팅, 단일 호스트 / 소규모 클러스터 변형입니다.
**nginx 핫 업그레이드**는 "신·구 공존 후 구 것을 드레인"의 교과서입니다:
`USR2`가 listen fd를 상속받은 새 master를 시작 → `WINCH`가 구 worker들을
정상 정지 → `QUIT`이 구 master를 은퇴시킵니다. §7의 흐름은 구조적으로
동일합니다.
## 3. 전체 아키텍처
설계는 하나의 우아한 골격으로 수렴합니다: **어디서나 감독 트리; 유일한 차이는
감독자가 대등 복제본인지 leader/follower인지입니다.**
```
                    ┌─────────────────────────────────────────┐
   공통 베이스       │ 시그널 시맨틱 · 드레인(drain)            │  ← 세 프로젝트
   (Layer 1 / 3)    │ /healthz · /readyz(drain 비트 포함)     │     모두 동일
                    │ socket activation + 3가지 배포 어댑터    │
                    └─────────────────────────────────────────┘
                                   │
          ┌────────────────────────┴────────────────────────┐
          ▼                                                  ▼
   ┌────────────────┐                              ┌──────────────────┐
   │ 서브시스템 A   │  supervisor는 PEER            │ 서브시스템 B     │  supervisor는
   │ Replica        │  (active-active)              │ Leader/Follower  │  leader 또는 follower
   │                │  ← 서버 LB + 롤링 업데이트    │                  │  ← 엣지 내결함성
   └───────┬────────┘                              └─────────┬────────┘
           │                                                  │
           └──────────────────┬───────────────────────────────┘
                              ▼
               ┌──────────────────────────────┐
               │ 통일된 Worker 추상화         │  ← 세 프로젝트의 모든
               │ 라이프사이클 FSM + 감독 재시작│     "자식 프로세스 자원"
               │ (permanent/transient)        │     cosmos / pglite-proxy / 프로토콜별 worker
               │ + 슬라이딩 윈도우 속도 제한   │
               └──────────────────────────────┘
```
### 3.1 핵심 단순화: 롤링 업데이트는 *세 번째* 시스템이 *아니다*
감독자 트리가 골격이 되면, 사용자의 요구는 세 개가 아닌 **두** 개의
서브시스템으로 붕괴됩니다:
- **서브시스템 A — Replica.** 서버 측에서 부하를 분담하는 N개의 동일한 대등
  인스턴스를 실행합니다. *로드 밸런싱과 롤링 업데이트는 동일한
  서브시스템입니다:* 롤링 업데이트는 단지 "복제본 수를 임시 +1 → 구 복제본
  하나 드레인 → 반복"입니다. 이것은 Kubernetes의 `maxSurge/maxUnavailable`
  연산을 복제본 추가/제거로 표현한 것입니다.
- **서브시스템 B — Leader/Follower.** 엣지(evernight 기기)에서 내결함성을
  위해 두 개의 프로세스, 하나의 leader와 하나의 follower를 실행합니다.
  leader가 물리적 I/O를 독점합니다; follower는 대기합니다.
별도의 "롤링 업데이트 시스템"은 존재하지 않습니다: 그것은 서브시스템 A의
유지보수 연산입니다(그리고, 다른 시맨틱으로, leader 페일오버를 통해 B에도
작용합니다).
### 3.2 계층 뷰
| 계층 | 서브시스템 A(Replica) | 서브시스템 B(Leader/Follower) | 공유? |
|---|---|---|---|
| **L1** 라이프사이클(시그널 / 드레인 / 프로브) | 동일 | 동일 | **공유** |
| **L3** 무정지 인계(socket activation) | 각 복제본이 systemd에서 fd 획득 | leader→follower fd 인계(고급) | **공유** |
| **L2** 조정 | **2a** 대등 레지스트리 + 공유 잠금(`pg_advisory`) | **2b** 리스 선출 + 배타적 자원 + leader/follower 레지스트리 | **분기**(동일 trait, 다른 정책) |
| **L4** 오케스트레이션 | **4a** 복제본 스케일 / 롤링 업데이트 | **4b** 페일오버 | **분기** |
통찰: **L1과 L3는 완전히 공유; L2/L4는 분기.** `CoordinationLock`은 2a와
2b에서 동일한 trait입니다 — A에서는 동시 쓰기 조정에, B에서는 leader 리스로
사용됩니다. 이 trait 통일이 바로 "원리가 공통이다"가 안착하는 지점입니다.
## 4. crate 소유권
"arona에 넣어라"라는 사용자 목표는 분할되어야 합니다. 왜냐하면 **현재 arona는
순수 프로토콜/타입 crate**이기 때문입니다 — `serde` / `ts-rs` / `schemars`
의존성만 있고, `lib.rs:5`는 "모든 타입이 entelecheia에서 정의되고
shittim-chest에서 소비된다"고 규정하며, crates.io 게시를 위해 모든 비
프로토콜 산출물을 `exclude`합니다. 런타임 로직(tokio, `sd_listen_fds`,
시그널 처리)을 주입하면 그 경량이고 게시 가능한 정체성을 훼손합니다.
분할:
- **`arona::lifecycle`(프로토콜 계약, arona에 배치).** JSON-RPC 메서드와
  타입만: `DrainState`, `ReadyStatus`, `Lifecycle.Drain`, `Lifecycle.Status`,
  `Worker.Status` 등. arona의 "양쪽에 쌍으로 존재" 규칙을 만족합니다.
- **`malkuth`(신규 crate, 런타임).** `arona` 프로토콜 타입 + `tokio`
  + `libsystemd` 바인딩(socket activation) + 백엔드 trait에 의존. feature
  게이트:
  - `replica` — 서브시스템 A 조정 + 오케스트레이션.
  - `leader-follower` — 서브시스템 B 리스 선출 + 페일오버.
  - `socket-activation` — systemd fd 획득.
  - `file-lock` / `pg-lock` / `lease` — `CoordinationLock` 백엔드.
세 프로젝트는 `malkuth`에 의존하고 필요한 feature를 활성화합니다
(§8 매트릭스 참조). 모든 것을 arona에 넣으면 그것을 "프로토콜 + 선택적
런타임"이 되도록 강제하고 순수성을 파괴합니다 — 권장하지 않습니다.
## 5. 핵심 추상화
### 5.1 `Worker` — 감독되는 자식 프로세스 자원
`Worker`는 독립적으로 종료 가능한 하나의 프로세스로, 정확히 하나의 자원(PLC
연결, 직렬 포트, 로컬 수신 포트, cosmos / pglite-proxy 같은 sidecar)을
보유합니다. 프로세스가 **장애 격리 경계**입니다: Modbus 스택의 버그가
S7comm worker를 오염시키지 않습니다.
라이프사이클 FSM(`evernight/src/model_server.rs:128-139`에서 발췌):
```
        시작                       health ok(정상)
 Starting ──────► Running ─────────────────► Running
     │              │  ▲
     │              │  │ health ok(자가 치유)
     │              ▼  │
     └──────► Failed ◄┘        크래시 / 비정상
                  │
                  │ 재시작 정책 = permanent
                  └────────► Starting(속도 제한)
```
### 5.2 `Supervisor` — worker 풀 보유
- **재시작 정책**(OTP 어휘): `permanent`(항상 재시작 — 자원 worker의
  기본값), `transient`(비정상 종료 시에만 재시작), `temporary`(재시작 안 함).
- **슬라이딩 윈도우 속도 제한**(entelecheia `health_daemon`의
  `max_restart_attempts` + `cooldown`에서 발췌): worker가 윈도우 W 내에 N회
  이상 재시작하면, 크래시 폭풍을 막기 위해 `cooldown`에 진입합니다; 이후
  재시작은 연기됩니다.
### 5.3 `Lifecycle` — 통일된 시그널 시맨틱(Layer 1)
nginx/Go 관행을 채택합니다:
| 시그널 | 시맨틱 | 동작 |
|---|---|---|
| `SIGINT`(ctrl_c) | SIGTERM과 동등(개발자 친화적) | 드레인 진입 |
| `SIGTERM` | **정상 종료** | 드레인: ready 비트 해제 → 수신 중지 → 진행 중인 작업 드레인 → 종료 |
| `SIGHUP` | **핫 설정 리로드** | 종료하지 않음; 설정 재읽기 |
| `SIGQUIT` | **즉시 종료**(긴급 전용) | 드레인 건너뜀, 빠른 종료 |
**드레인 시퀀스**(한 가지 구현 예; 각 프로젝트는 자체 "드레인 클로저"를
주입):
1. `/readyz`의 `draining = true` 설정(LB / 오케스트레이터가 이를 보고 새
   트래픽 전송을 중지).
2. 새 연결에 대한 `accept` 중지(socket activation 하에서: 상속된 fd에서의
   수신 중지).
3. 활성 WebSocket에 close 프레임 전송; 타임아웃 `DRAIN_TIMEOUT`(기본 30s,
   설정 가능)으로 진행 중인 요청 대기.
4. 백그라운드 태스크 드레인(entelecheia `TaskManager.stop_all` + `wait_all`
   복사).
5. 업스트림 풀을 깔끔하게 연결 해제(shittim-chest `upstream_pool`의 깔끔한
   연결 해제 복사).
6. 잠금 해제, 임시 파일 정리 → exit 0.
구현 메모: axum의 `axum::serve(listener, app).with_graceful_shutdown(...)`은
이미 드레인을 지원합니다; **핵심 누락 조각은 `SIGTERM`을 연결하는 것**
입니다(오늘날 `ctrl_c`만 연결됨). 참조:
`entelecheia/.../shutdown.rs:17`, `shittim-chest/.../api.rs:465`,
`evernight/src/api/mod.rs:109`.
### 5.4 헬스 엔드포인트(통일)
프로브 분리(오늘날 세 프로젝트가 불일치):
| 엔드포인트 | 시맨틱 | 판정 |
|---|---|---|
| `/healthz`(liveness) | 프로세스가 살아 있음 | 프로세스가 응답 가능하면 200(단순 재시작 판정 기준) |
| `/readyz`(readiness) | **서비스 가능**, drain 비트 포함 | 드레인 중이 아니고 의존성이 준비됨(DB ping / scepter 소켓 / 첫 스테이션 폴링)이면 200; 드레인 중이면 503 |
`/readyz`의 `draining` 비트는 롤링 업데이트의 핵심 신호입니다:
오케스트레이터는 `/readyz`가 200인 인스턴스로만 새 요청을 라우팅합니다.
shittim-chest의 기존 `GET /api/health`(`routes.rs:27`)는 drain 비트가 있는
`/readyz`로 업그레이드됩니다.
### 5.5 `acquire_listener` — Layer 3 무정지 인계
`malkuth`는 `acquire_listener(addr) -> TcpListener`를 노출합니다:
1. 우선 `sd_listen_fds()` 시도(`LISTEN_PID` 검증) — systemd가 fd를 보유 중.
2. 일반 `TcpListener::bind(addr)`로 폴백(dev, systemd 없음).
axum `serve(listener, ...)`은 이미 사전 바인딩된 리스너를 수용하므로 배관은
갖춰져 있습니다; 오늘날 fd의 *소스*만 누락되어 있습니다. 세 가지 배포
어댑터:
| 배포 | 방식 | 적용 |
|---|---|---|
| **bare systemd** | `xxx.socket` + `xxx@.service` 템플릿 인스턴스 | scepter, evernight-gateway, malkuth 자신 |
| **docker**(shittim-chest 운영) | 호스트 systemd socket activation, 바인딩된 소켓/fd를 컨테이너로 전달(`LISTEN_FDS` + `SocketUser`); 또는 컨테이너 내에서 fd를 보유하는 경량 master | shittim-chest 운영 |
| **dev** | 일반 `bind` 폴백 + 짧은 중첩(수백 ms의 연결 유실 수용), systemd 없음 | 세 프로젝트 dev |
socket activation 하의 롤링 업데이트:
```
[업그레이드 트리거] → 신규 인스턴스 시작(템플릿화된 service@new, fd 상속/재취득)
                   → 신규 인스턴스 /readyz가 200이 될 때까지 폴링
                   → 구 인스턴스에 SIGTERM 전송(= 드레인)
                   → 구 인스턴스가 드레인 후 자체 종료
                   → systemd가 fd를 계속 보유 → 연결 유실 0건
```
### 5.6 `CoordinationLock` — 백엔드가 있는 Layer 2 trait
롤링 업데이트 윈도우 동안 구·신 인스턴스가 공유 자원을 동시에 읽고 쓸 수
있습니다. DB 트랜잭션은 자연스럽게 안전합니다; 파일(evernight JSONL,
설정)은 "쓰기 전 알림 + 잠금"이 필요합니다.
```rust
pub trait CoordinationLock: Send + Sync {
    async fn acquire(&self, key: &str, lease: Duration) -> Result<LockGuard>;
}
// 백엔드:
//   FileLock  — flock/fcntl,       evernight(JSONL / 설정)용
//   PgLock    — pg_advisory_lock,  entelecheia / shittim-chest용
//   LeaseLock — 파일 잠금 + 리스(크래시 시 자동 만료)
```
**인스턴스 레지스트리**(업그레이드 윈도우 동안만 사용; 안정 상태에서는 단일
레코드): `{instance_id, role: Active | Draining, started_at, generation}`을
기록하는 작은 공유 테이블/파일. 신규 인스턴스는 시작 시 `Active` 행을
기록합니다; 업그레이드 시 구 인스턴스는 `Draining`으로 표시됩니다. 이것은
명시적으로 범위 밖이었던 Raft 정족수를 대체합니다 — 안정 상태가 단일
인스턴스이므로 레지스트리는 드레인만 조정하며, 강일관성 필요가 없습니다
(파일이나 DB 행이면 충분합니다).
## 6. 서브시스템 A — Replica(로드 밸런싱 ⊃ 롤링 업데이트)
**형태.** 전방 LB 뒤에서 병렬 실행되는 N개의 동일한 대등 인스턴스, 상태는
공유 Postgres에. **active-active**, 인스턴스는 대등하며 leader가 없습니다.
| 관심사 | 방식 |
|---|---|
| 요청 라우팅 | 전방 LB(caddy / 내장 `SO_REUSEPORT` 라운드로빈); `/readyz`로 제거 |
| 공유 상태 R/W | DB 트랜잭션 + 동시 쓰기 조정을 위한 `pg_advisory_lock`(자연스럽게 안전) |
| WebSocket / 긴 연결 | **스티키 세션**(LB가 cookie/instance id로 고정) 또는 **세션 마이그레이션**(연결 해제 후 클라이언트가 임의 복제본에 재연결, 상태는 DB에서 복구 — shittim-chest `upstream_pool`이 이미 재연결 템플릿) |
| 세션 어피니티 | entelecheia와 shittim-chest 모두 상태를 Postgres로 외부화하고 부팅 시 복구 → **자연스럽게 복제본 친화적**, evernight 대비 핵심 이점 |
| 롤링 업데이트 | 복제본 스케일 서브 연산: 신규 복제본(신규 버전) 추가 → ready → 구 복제본 드레인 및 제거 → 반복. 즉 K8s `maxSurge/maxUnavailable`의 축소판 |
**A가 비교적 쉬운 이유.** entelecheia와 shittim-chest가 상태를 Postgres로
외부화하기 때문에(부팅 시 복구, 감사로 확인됨) 복제본 간 상태 복제가 필요
없습니다 — 전방 LB와 DB 트랜잭션이면 충분합니다. 유일하게 어려운 부분은
WebSocket 스티키 / 마이그레이션입니다.
## 7. 서브시스템 B — Leader/Follower(엣지 active-passive HA)
**형태.** 동일 기기/게이트웨이의 두 evernight 프로세스, 하나의 leader와
하나의 follower; 업스트림 `evernight-server`는 **하나의 `node_id`**(하나의
기기)를 봅니다. **active-passive**, 인스턴스는 대등하지 않으며, leader가
물리적 I/O를 독점합니다.
### 7.1 B를 단순화하는 "트릭": 감독 트리 + let-it-crash
모든 자원에 내결함성을 부여하는 대신, **오직 감독자만 leader/follower로
만듭니다**; 자원은 크래시 시 단순히 재시작되는 독립적인 worker 프로세스입니다.
구체적으로(합의된 결정에 따라):
```
supervisor  (leader / follower HA)        ← 오직 이 레이어만 리스 선출 + 페일오버 수행
   ├─ worker: PLC-A (Modbus)              ← 감독 재시작, 단일 인스턴스
   ├─ worker: PLC-B (S7comm)              ← 감독 재시작, 단일 인스턴스
   ├─ worker: serial / CAN                ← 감독 재시작, 단일 인스턴스
   └─ worker: local port listener         ← 감독 재시작, 단일 인스턴스
```
이것이 OTP 감독 트리 / "let it crash" 모델입니다(또한 systemd
`Restart=always`, K8s Pod). 이점:
- **관심사 분리.** worker는 "하나의 자원을 보유하고 일한다"만 수행합니다;
  내결함성, 선출, 상태 동기화 로직을 떠안지 않습니다 — 최대로 단순하게
  유지됩니다.
- **내결함성 집중.** 오직 감독자만 HA를 수행합니다; 복잡도가 한 곳으로
  수렴합니다.
- **장애 격리.** 한 자원(예: 한 PLC 프로토콜)의 크래시가 다른 것에 영향을
  주지 않습니다(별개의 프로세스).
- **프로토콜 적합성.** evernight는 많은 산업 프로토콜(Modbus/S7/CAN/직렬)을
  사용합니다; 각각을 worker로 매핑하면 격리 가치가 최대가 됩니다.
### 7.2 페일오버 시 worker 라이프사이클 — 자식 프로세스 모델(합의된 출발점)
worker는 감독자의 **자식 프로세스**입니다(`kill_on_drop`). leader 사망 →
worker가 고아/종료됨 → 승격된 follower가 **모든 worker를 재생성**(각 PLC가
재연결).
- 가장 단순한 모델; "서버가 책임진다"는 의도에 부합합니다.
- 비용: 감독자 페일오버 시 모든 자원이 잠시 끊기고 재연결됩니다.
- 고급(연기): worker를 더 하위 init 하의 독립 데몬으로, 감독자는 IPC로만
  지시; 신규 감독자가 생존 worker를 `attach`. 중단은 더 적지만 worker가
  "현재 감독자에 재바인드"를 구현해야 합니다 — 더 복잡합니다. 향후 옵션으로
  기록됩니다.
### 7.3 leader 선출 + 펜싱
- **리스 선출.** leader가 파일 잠금 + 리스(TTL 포함)를 보유하고, 매
  하트비트마다 갱신; follower는 폴링; leader 하트비트 타임아웃 시 follower가
  리스를 빼앗고 자신을 승격합니다.
- **배타적 물리 자원.** PLC/직렬/CAN 연결은 leader만 보유할 수 있습니다(두
  프로세스가 같은 PLC를 폴링하면 충돌) → leader가 폴링, follower는 대기.
  이것이 B가 active-active가 아닌 active-passive여야 하는 근본 이유입니다.
- **상태 동기.** 출발점은 **콜드 스탠바이**(follower는 복제하지 않음;
  승격 시 디스크의 JSONL에서 복구). 핫 스탠바이(follower가 leader의 JSONL을
  따라감)는 고급 옵션입니다.
- **스플릿브레인 펜싱.** 리스 TTL + 펜싱: follower는 리스가 진정으로
  만료된 후에만 빼앗을 수 있습니다; 빼앗은 후 구 leader의 추가 쓰기를
  물리적으로 차단합니다(물리 I/O 배타성이 자연스러운 펜스입니다).
- **단일 기기 정체.** leader와 follower가 하나의 `node_id`를 공유합니다;
  현재 leader만 `device.register`를 송신합니다.
고전적 유사체: keepalived/VRRP, DRBD+Pacemaker, MySQL primary/replica, Redis
Sentinel — 기기 내, 프로세스 수준의 단순화 버전으로. 이론(리스 선출 +
펜싱)은 탄탄한 기반 위에 있습니다.
## 8. 프로젝트별 도입 매트릭스
| 프로젝트 | 감독자 역할 | worker | 전략 |
|---|---|---|---|
| entelecheia(scepter) | 복제본 중 하나 | cosmos sidecar, agent 컨테이너 | **A Replica** |
| shittim-chest(chest) | 복제본 중 하나 | pglite-proxy(mock), channel intake | **A Replica** |
| evernight 기기(`sensor-poll`) | **leader / follower** | 프로토콜당 하나의 worker(Modbus/S7/CAN/직렬) | **B Leader/Follower** |
| evernight-server(중앙) | 복제본 중 하나 | model_server 컨테이너 | **A Replica** |
프로젝트별 feature 선택(`malkuth`):
- entelecheia / shittim-chest / evernight-server: `replica` +
  `socket-activation` + `pg-lock`; sidecar를 위한 worker 추상화.
- evernight 기기: `leader-follower` + `socket-activation`(고급 fd 인계) +
  `file-lock` / `lease`; 프로토콜당 프로세스를 위한 worker 추상화.
## 9. 출시 단계
1. **단계 A — 세 프로젝트에 걸친 Layer 1.** 시그널 시맨틱 + `/healthz` /
   `/readyz` + 드레인. 가장 낮은 위험, 가장 즉각적인 보상(SIGTERM 하드 킬을
   먼저 수정).
2. **단계 B — `arona::lifecycle` 프로토콜 + `malkuth` 골격.** trait
   정의, `acquire_listener`, `CoordinationLock` trait + `FileLock` / `PgLock`
   백엔드, `Worker` + `Supervisor` 원시 타입.
3. **단계 C — Layer 3.** 세 프로젝트를 위한 socket activation 유닛 + docker
   어댑터 + dev 폴백.
4. **단계 D — Layer 4.** 매니페스트 큐 오케스트레이터(`health_daemon` 복사) +
   구·신 공존 드레인 루프, dev "신규 서버 컴파일" 워크플로 포함; 더불어 B를
   위한 leader/follower 페일오버.
## 10. 위험 및 경계
- **shittim-chest docker + socket activation**이 가장 불확실한 조각입니다;
  fd-into-컨테이너 인계는 프로토타입 스파이크가 필요합니다. 불가능하면
  "외부 caddy + `/readyz` 제거 + 짧은 중첩"으로 폴백.
- **evernight 메모리 내 상태**(`DeviceRegistry`, 세션)는 드레인 시 손실됩니다;
  영속화 또는 마이그레이션 여부를 결정해야 합니다(최악: 신규 인스턴스가
  재구축, 긴 세션이 끊김).
- **WebSocket 드레인 ≠ 마이그레이션.** 구 인스턴스 종료 시 진행 중 WS는
  여전히 끊깁니다; "매끄러움"은 클라이언트가 신규 인스턴스에 재연결해야
  합니다(shittim-chest 클라이언트는 이미 `upstream_pool` 재연결 로직을
  보유, 재사용 가능).
- **프로세스 수준 vs 스레드 수준 내결함성.** Leader/Follower(서브시스템 B)는
  *프로세스/인스턴스 수준* 내결함성을 해결하지, 스레드 수준은 아닙니다.
  태스크 내 tokio panic은 감독자의 역할입니다(태스크 재시작, 프로세스
  페일오버가 아님). B를 스레드 크래시 흡수에 사용하지 마십시오 — 너무
  무겁습니다.
- **명시적으로 범위 밖.** Raft 정족수, 일관 해시 샤딩, 크로스 데이터센터 HA
  — "롤링 업데이트 + 엣지 HA" 범위에 의해 제외됩니다. 단일 인스턴스 안정
  상태는 레지스트리가 드레인만 조정함을 의미합니다.
## 11. 미해결 질문(연기)
- 페일오버 시 worker: 출발점으로 자식 프로세스 모델을 선택; "독립 데몬 +
  attach" 모델은 고급으로 기록됩니다.
- B의 콜드 vs 핫 스탠바이: 출발점으로 콜드를 선택(승격 시 JSONL에서
  복구); 핫(follower가 leader 로그를 따라감)은 연기됩니다.
- entelecheia의 `cosmos` sidecar와 shittim-chest의 `pglite-proxy`를 통일된
  `Worker` 추상화 아래에도 래핑할지: 동의함(세 프로젝트에 걸쳐 하나의 worker
  추상화로 통일).
---
*영문 정본(canonical): `docs/en/design/platform/supervision-and-rolling-update.md`.
다른 언어(zht/ja/fr/es/ru)의 번역은 i18n 대기 중입니다.*
