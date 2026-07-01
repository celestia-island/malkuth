# 빠른 시작

## 의존성 추가

```toml
[dependencies]
malkuth = { git = "https://github.com/celestia-island/malkuth.git", branch = "dev" }
# features: tcp (default) | ws | ipc | signals (default) | worker | probes |
#           file-lock | lease | pg-lock | replica | leader-follower | schema
```

## 최소 JSON-RPC 서비스

```rust
use std::sync::Arc;
use malkuth::{Router, Supervised};
use malkuth::transport::TcpTransport;
use malkuth::Transport;
use serde_json::json;

#[tokio::main]
async fn main() -> std::io::Result<()> {
    let lis = TcpTransport.listen("tcp://127.0.0.1:0").await?;
    let supervised = Supervised::new().signals();
    let ctrl = supervised.drain_controller();

    let handler = Arc::new(
        Router::new()
            .lifecycle(ctrl, None)            // registers Lifecycle.Drain / Status / Health / Reload
            .route("ping", |_| Box::pin(async { Ok(json!("pong")) })),
    );

    supervised.serve_rpc_listener(lis, handler).await
}
```

`Supervised`는 JSON-RPC 서버를 OS 시그널 종료 소스
(SIGINT/SIGTERM → 드레인, SIGHUP → 리로드, SIGQUIT → 즉시 종료)와 경쟁시킨 뒤,
등록된 드레인 훅들을 실행합니다. `.signals()`를 `.exit(your_impl)`로
바꾸면 자체 로직에서 드레인을 트리거할 수 있습니다.

## 클라이언트에서 호출

```rust
use malkuth::Client;
use malkuth::transport::TcpTransport;
use malkuth::Transport;
use serde_json::json;

let mut c = Client::connect(&TcpTransport, "tcp://127.0.0.1:8080").await?;

// Custom method:
let r = c.call("ping", json!({})).await?;       // → "pong"

// Standard lifecycle methods (registered by Router::lifecycle):
c.notify("Lifecycle.Drain", json!({})).await?;  // → server begins graceful drain
let health = c.call("Lifecycle.Health", json!({})).await?;
// → { "alive": true, "pid": 12345, "uptime_secs": 360, "version": "0.1.0" }
let status = c.call("Lifecycle.Status", json!({})).await?;
// → { "ready": true, "draining": false, "dependencies": [], "generation": null }
```

## JSON-RPC 라이프사이클 프로토콜

`Router::lifecycle(drain, probe)`는 네 가지 표준 메서드를 등록합니다:

| 메서드 | 매개변수 | 결과 | 효과 |
| --- | --- | --- | --- |
| `Lifecycle.Drain` | `{}` | `{ "accepted": true, "draining": true }` | 우아한 드레인 시작 |
| `Lifecycle.Reload` | `{}` | `null` | 리로드 시작(종료하지 않음) |
| `Lifecycle.Status` | `{}` | `ReadyStatus` | 준비 상태 조회(드레인 비트 + 의존성) |
| `Lifecycle.Health` | `{}` | `HealthStatus` | 활성 상태 조회(pid / 업타임 / 버전) |

모든 메시지는 선택한 트랜스포트 상에서 NDJSON 프레이밍된 JSON-RPC 2.0으로
전달됩니다.

## 다른 트랜스포트

`TcpTransport`를 `WsTransport`(feature `ws`, 주소 `ws://host:port`)나
`IpcTransport`(feature `ipc`, 주소 `ipc:/tmp/sock`)로 교체할 수 있습니다.
또는 URL scheme(`tcp://` / `ws://` / `ipc:`)로 디스패치하는
`MultiTransport`를 사용하세요.

## CLI로 임의 프로그램 감싸기

```bash
malkuth --watch ./src --proxy 3000:3000-3999 --pod-count 3 -- cargo run
```

이 명령은 3개의 포드를 실행하고(`PORT` 환경 변수로 포트 3001~3003을 자체 할당),
각 포드가 리스닝을 시작할 때까지 프로브하며, 포트 3000에서 스티키 리버스 프록시를
전면에 배치합니다(클라이언트 IP 기반 일관적 해시 라우팅). `./src` 아래의 변경은
롤링 재시작을 트리거하며 한 번에 하나의 포드씩 재시작합니다.
