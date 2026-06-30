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

### `file-lock` (POSIX `flock`)

Enable the `file-lock` feature:

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

One lock file per `key`, created under the root directory. Uses `flock(LOCK_EX | LOCK_NB)`
for non-blocking exclusive locks. If another process holds the lock, returns
`LockError::Contended`.

> **Unix-only.** `FileLock` uses POSIX `flock` and is only available on Unix
> targets (Linux, macOS, BSD). It is not available on Windows.

### `lease` (file lock with TTL)

Staged — not yet implemented. Will provide crash-resilient file locks with
automatic TTL expiry, building on `file-lock`.

### `pg-lock` (PostgreSQL advisory lock)

Staged — not yet implemented. Will use `pg_advisory_lock` for distributed
coordination across multiple hosts sharing a Postgres instance.

## When to use which

| Scenario | Backend |
| --- | --- |
| Single host, lock holder won't crash | `file-lock` |
| Single host, lock holder might crash | `lease` (staged) |
| Multiple hosts, shared Postgres | `pg-lock` (staged) |
| Multiple hosts, no shared DB | External (etcd, Consul) |
