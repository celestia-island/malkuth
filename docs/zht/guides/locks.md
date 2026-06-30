# 協調鎖

## 抽象

`CoordinationLock` 是一個可插拔的 trait，用於跨行程的互斥。它是兩種容錯
策略的共享原語：

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

### `file-lock`（POSIX `flock`）

啟用 `file-lock` 功能：

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

每個 `key` 對應一個鎖檔案，建立在根目錄下。使用 `flock(LOCK_EX | LOCK_NB)`
進行非阻塞的排他鎖。如果另一個行程持有該鎖，則回傳 `LockError::Contended`。

### `lease`（帶 TTL 的檔案鎖）

啟用 `lease` 功能（隱含 `file-lock`）：

```toml
malkuth = { features = ["lease"] }
```

API 與 `file-lock` 相同，但如果鎖持有者崩潰，租約會在 TTL 之後過期，
另一個行程就可以取得它。適用於鎖持有者可能在未釋放的情況下死掉的单機部署。

### `pg-lock`（PostgreSQL 諮詢鎖）

已列入計畫 —— 尚未實作。將使用 `pg_advisory_lock` 在共享一個 Postgres 實例的
多個主機之間進行分散式協調。

## 何時使用哪個

| 場景 | 後端 |
| --- | --- |
| 單機，鎖持有者不會崩潰 | `file-lock` |
| 單機，鎖持有者可能崩潰 | `lease` |
| 多機，共享 Postgres | `pg-lock`（已列入計畫） |
| 多機，無共享資料庫 | 外部（etcd、Consul） |
