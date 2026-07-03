# 协调锁

## 抽象

`CoordinationLock` 是一个可插拔的 trait，用于跨进程的互斥。它是两种容错
策略共享的原语：

- **Replica**（子系统 A）—— 协调对共享状态的并发写入。
- **Leader/Follower**（子系统 B）—— 将锁用作 leader 租约。

## 该 trait

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

## 后端

### `FileLock`（POSIX `flock`）—— feature `file-lock`

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

每个 `key` 对应一个锁文件，创建在根目录下。使用
`flock(LOCK_EX | LOCK_NB)` 进行非阻塞的排他锁。如果另一个进程持有该锁，
则返回 `LockError::Contended`。

> **仅限 Unix。** `FileLock` 使用 POSIX `flock`，仅在 Unix 目标
> （Linux、macOS、BSD）上可用。

### `LeaseLock`（带 TTL 的文件租约）—— feature `lease`

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

使用原子的临时文件重命名（CAS）来获取租约。一个后台线程以 TTL/3 的间隔
续约。崩溃时，租约文件中的 `expires_at_ms` 时间戳允许下一个获取者接管。

### `PgLock`（PostgreSQL 咨询锁）—— feature `pg-lock`

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

在一个由 `key` 字符串经 FNV-1a 哈希派生的 bigint 键上使用会话级
`pg_try_advisory_lock` / `pg_advisory_unlock`。

## 何时使用哪个

| 场景 | 后端 |
| --- | --- |
| 单机，锁持有者不会崩溃 | `FileLock` |
| 单机，锁持有者可能崩溃 | `LeaseLock` |
| 多机，共享 Postgres | `PgLock` |
| 多机，无共享数据库 | 外部（etcd、Consul） |
