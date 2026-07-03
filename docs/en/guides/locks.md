# Coordination Locks

## The abstraction

`CoordinationLock` is a pluggable trait for mutual exclusion across processes.
It is the shared primitive for both fault-tolerance strategies:

- **Replica** (Subsystem A) — coordinate concurrent writes to shared state.
- **Leader/Follower** (Subsystem B) — use the lock as the leader lease.

## The trait

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

## Backends

### `FileLock` (POSIX `flock`) — feature `file-lock`

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

One lock file per `key`, created under the root directory. Uses
`flock(LOCK_EX | LOCK_NB)` for non-blocking exclusive locks. If another process
holds the lock, returns `LockError::Contended`.

> **Unix-only.** `FileLock` uses POSIX `flock` and is only available on Unix
> targets (Linux, macOS, BSD).

### `LeaseLock` (file lease with TTL) — feature `lease`

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

Uses atomic temp-file rename (CAS) to claim the lease. A background thread
renews it at TTL/3 intervals. On crash, the lease file's `expires_at_ms`
timestamp allows the next acquirer to take over.

### `PgLock` (PostgreSQL advisory lock) — feature `pg-lock`

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

Uses session-level `pg_try_advisory_lock` / `pg_advisory_unlock` on a bigint
key derived from the `key` string via FNV-1a hash.

## When to use which

| Scenario | Backend |
| --- | --- |
| Single host, lock holder won't crash | `FileLock` |
| Single host, lock holder might crash | `LeaseLock` |
| Multiple hosts, shared Postgres | `PgLock` |
| Multiple hosts, no shared DB | External (etcd, Consul) |
