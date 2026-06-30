# 리스너 인계

## 문제점

서버 프로세스가 재시작할 때, 아무도 포트를 리스닝하지 않는 기간이 존재하여 ——
들어오는 연결이 버려집니다. 무정지 롤링 업데이트를 위해서는 새 프로세스가
이전 프로세스로부터 리스닝 소켓을 상속받아야 합니다.

## 해결책: 소켓 활성화

systemd(또는 커스텀 런처)가 리스닝 소켓 fd를 열어둡니다. 프로세스가 재시작할 때,
새 프로세스가 fd를 상속받아 즉시 연결을 수락할 수 있습니다 —— 커널이 그 사이의
공백 동안 연결을 큐에 보관합니다.

Malkuth의 `acquire_listener`는 이를 **순수 Rust**로 구현합니다(`libsystemd` 불필요):

```rust
use malkuth::acquire_listener;

// Prefers systemd socket activation (fd inherited), falls back to a plain bind.
let listener = acquire_listener("0.0.0.0:8080").await?;
```

`socket-activation` 기능을 활성화합니다:

```toml
[dependencies]
malkuth = { git = "https://github.com/celestia-island/malkuth.git", branch = "dev", features = ["socket-activation"] }
```

## 작동 방식

systemd는 두 개의 환경 변수를 설정합니다:

| 변수 | 의미 |
| --- | --- |
| `LISTEN_PID` | fd를 상속받아야 할 프로세스의 PID(자신의 PID와 같아야 함) |
| `LISTEN_FDS` | 전달되는 fd의 수(fd 3부터 시작) |

Malkuth는 이들을 읽고, `LISTEN_PID == our_pid`를 검증한 뒤, fd 3
(`SD_LISTEN_FDS_START`)의 소유권을 가져와 논블로킹으로 설정하고,
`tokio::net::TcpListener`로 감쌉니다.

변수가 없거나 PID가 일치하지 않으면 `TcpListener::bind(addr)`로 폴백합니다.

## systemd 유닛 예시

```ini
# /etc/systemd/system/myapp.socket
[Socket]
ListenStream=8080

[Install]
WantedBy=sockets.target
```

```ini
# /etc/systemd/system/myapp.service
[Service]
ExecStart=/usr/bin/myapp
# systemd passes the socket fd automatically when the socket unit is active
```

이 설정을 사용하면, `systemctl restart myapp`가 처리 중인 연결을 버리지 않습니다:
새 프로세스가 시작되어 fd를 상속받는 동안 커널이 그것들을 리슨 큐에 보관합니다.
