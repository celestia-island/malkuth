# 조정 잠금

## 추상화

`CoordinationLock`은 프로세스 간 상호 배제를 위한 플러그 가능한 트레이트입니다.
이것은 두 가지 내결함성 전략의 공유 원시입니다:

- **Replica**(서브시스템 A) —— 공유 상태에 대한 동시 쓰기를 조정합니다.
- **Leader/Follower**(서브시스템 B) —— 잠금을 리더 리스로 사용합니다.

## 이 트레이트

```rust
#[async_trait]
pub trait CoordinationLock: Send + Sync {
    async fn acquire(&self, key: &str, lease: Duration)
        -> Result<Box<dyn LockGuard>, LockError>;
}

#[async_trait]
pub trait LockGuard: Send + Sync {
    async fn release(&mut self);
}
```

## 백엔드

### `file-lock`(POSIX `flock`)

`file-lock` 기능을 활성화합니다:

```toml
malkuth = { features = ["file-lock"] }
```

```rust
use malkuth::lock::FileLock;
use std::time::Duration;

let lock = FileLock::new("/var/lib/myapp/locks");

let mut guard = lock.acquire("write-queue", Duration::from_secs(30)).await?;
// ... exclusive work ...
guard.release().await; // or just drop guard
```

각 `key`마다 하나의 잠금 파일이 루트 디렉터리 아래에 생성됩니다. 논블로킹 배타적 잠금을 위해
`flock(LOCK_EX | LOCK_NB)`를 사용합니다. 다른 프로세스가 잠금을 보유하고 있으면
`LockError::Contended`를 반환합니다.

### `lease`(TTL이 있는 파일 잠금)

`lease` 기능을 활성화합니다(`file-lock`을 암시적으로 포함):

```toml
malkuth = { features = ["lease"] }
```

API는 `file-lock`과 동일하지만, 잠금 보유자가 충돌하면 리스가 TTL 이후에 만료되어
다른 프로세스가 획득할 수 있습니다. 잠금 보유자가 해제 없이 죽을 수 있는 단일 호스트
배포에 유용합니다.

### `pg-lock`(PostgreSQL 어드바이저리 잠금)

계획 중 —— 아직 구현되지 않았습니다. 하나의 Postgres 인스턴스를 공유하는 여러 호스트 간의
분산 조정을 위해 `pg_advisory_lock`을 사용할 예정입니다.

## 언제 무엇을 사용할지

| 시나리오 | 백엔드 |
| --- | --- |
| 단일 호스트, 잠금 보유자가 충돌하지 않음 | `file-lock` |
| 단일 호스트, 잠금 보유자가 충돌할 수 있음 | `lease` |
| 여러 호스트, 공유 Postgres | `pg-lock`(계획 중) |
| 여러 호스트, 공유 DB 없음 | 외부(etcd, Consul) |
