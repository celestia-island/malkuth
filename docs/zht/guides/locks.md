# 協調鎖

## 抽象

`CoordinationLock` 是一個可插拔的 trait，用於跨行程的互斥。它是兩種容錯
策略共享的原語：

- **Replica**（子系統 A）—— 協調對共享狀態的並發寫入。
- **Leader/Follower**（子系統 B）—— 將鎖用作 leader 租約。

## 該 trait

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

## 後端

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

每個 `key` 對應一個鎖檔案，建立在根目錄下。使用
`flock(LOCK_EX | LOCK_NB)` 進行非阻塞的排他鎖。如果另一個行程持有該鎖，
則回傳 `LockError::Contended`。

> **僅限 Unix。** `FileLock` 使用 POSIX `flock`，僅在 Unix 目標
> （Linux、macOS、BSD）上可用。

### `LeaseLock`（帶 TTL 的檔案租約）—— feature `lease`

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

使用原子的暫存檔重新命名（CAS）來取得租約。一個背景執行緒以 TTL/3 的間隔
續約。崩潰時，租約檔案中的 `expires_at_ms` 時間戳允許下一個取得者接管。

### `PgLock`（PostgreSQL 諮詢鎖）—— feature `pg-lock`

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

在一個由 `key` 字串經 FNV-1a 雜湊派生的 bigint 鍵上使用工作階段層級的
`pg_try_advisory_lock` / `pg_advisory_unlock`。

## 何時使用哪個

| 情境 | 後端 |
| --- | --- |
| 單機，鎖持有者不會崩潰 | `FileLock` |
| 單機，鎖持有者可能崩潰 | `LeaseLock` |
| 多機，共享 Postgres | `PgLock` |
| 多機，無共享資料庫 | 外部（etcd、Consul） |
