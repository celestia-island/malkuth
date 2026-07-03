# 조정 락

## 추상화

`CoordinationLock`은 프로세스 간 상호 배제를 위한 플러그 가능한 트레이트입니다.
두 가지 내결함성 전략이 공유하는 원시 타입입니다:

- **Replica**(서브시스템 A) —— 공유 상태에 대한 동시 쓰기를 조정.
- **Leader/Follower**(서브시스템 B) —— 락을 리더 리스로 사용.

## 트레이트

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

### `FileLock`(POSIX `flock`) —— feature `file-lock`

```toml
malkuth = { features = ["file-lock"] }
```

```rust
use malkuth::lock::FileLock;
use malkuth::CoordinationLock;
use std::time::Duration;

let lock = FileLock::new("/var/lib/myapp/locks");
let mut guard = lock.acquire("write-queue", Duration::from_secs(30)).await?;
// ... exclusive work ...
guard.release().await; // or just drop guard
```

`key`마다 하나의 락 파일이 루트 디렉토리 아래에 생성됩니다. 비차단 배타 락을 위해
`flock(LOCK_EX | LOCK_NB)`을 사용합니다. 다른 프로세스가 락을 쥐고 있으면
`LockError::Contended`를 반환합니다.

> **Unix 전용.** `FileLock`은 POSIX `flock`을 사용하며 Unix 타깃
> (Linux, macOS, BSD)에서만 사용할 수 있습니다.

### `LeaseLock`(TTL이 있는 파일 리스) —— feature `lease`

```toml
malkuth = { features = ["lease"] }
```

```rust
use malkuth::lease::LeaseLock;
use malkuth::CoordinationLock;

let lock = LeaseLock::new("/var/lib/myapp/leases");
let mut guard = lock.acquire("device-leader", Duration::from_secs(10)).await?;
// The lease auto-renews in the background. If the process crashes,
// the lease expires after the TTL and another process can acquire it.
guard.release().await;
```

원자적 임시 파일 리네임(CAS)으로 리스를 획득합니다. 백그라운드 스레드가
TTL/3 간격으로 갱신합니다. 크래시 시 리스 파일의 `expires_at_ms` 타임스탬프가
다음 획득자가 인계받을 수 있게 합니다.

### `PgLock`(PostgreSQL 어드바이저리 락) —— feature `pg-lock`

```toml
malkuth = { features = ["pg-lock"] }
```

```rust
use malkuth::pg_lock::PgLock;
use malkuth::CoordinationLock;
use tokio_postgres::NoTls;

let (client, connection) = tokio_postgres::connect("host=localhost dbname=myapp", NoTls).await?;
let lock = PgLock::new(std::sync::Arc::new(client));
let mut guard = lock.acquire("shared-config", Duration::from_secs(30)).await?;
guard.release().await;
```

`key` 문자열을 FNV-1a 해시로부터 파생한 bigint 키에 대해 세션 수준의
`pg_try_advisory_lock` / `pg_advisory_unlock`을 사용합니다.

## 무엇을 언제 쓸까

| 시나리오 | 백엔드 |
| --- | --- |
| 단일 호스트, 락 홀더가 크래시되지 않음 | `FileLock` |
| 단일 호스트, 락 홀더가 크래시될 수 있음 | `LeaseLock` |
| 다중 호스트, 공유 Postgres | `PgLock` |
| 다중 호스트, 공유 DB 없음 | 외부(etcd, Consul) |
