# 协调锁

## 抽象

`CoordinationLock` 是一个可插拔的 trait，用于跨进程的互斥。它是两种容错
策略的共享原语：

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

### `file-lock`（POSIX `flock`）

启用 `file-lock` 功能：

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

每个 `key` 对应一个锁文件，创建在根目录下。使用 `flock(LOCK_EX | LOCK_NB)`
进行非阻塞的排他锁。如果另一个进程持有该锁，则返回 `LockError::Contended`。

### `lease`（带 TTL 的文件锁）

启用 `lease` 功能（隐含 `file-lock`）：

```toml
malkuth = { features = ["lease"] }
```

API 与 `file-lock` 相同，但如果锁持有者崩溃，租约会在 TTL 之后过期，
另一个进程就可以获取它。适用于锁持有者可能在未释放的情况下死掉的单机部署。

### `pg-lock`（PostgreSQL 咨询锁）

已列入计划 —— 尚未实现。将使用 `pg_advisory_lock` 在共享一个 Postgres 实例的
多个主机之间进行分布式协调。

## 何时使用哪个

| 场景 | 后端 |
| --- | --- |
| 单机，锁持有者不会崩溃 | `file-lock` |
| 单机，锁持有者可能崩溃 | `lease` |
| 多机，共享 Postgres | `pg-lock`（已列入计划） |
| 多机，无共享数据库 | 外部（etcd、Consul） |
