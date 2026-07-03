# 調整ロック

## 抽象

`CoordinationLock` は、プロセス間の相互排他用のプラグ可能なトレイトです。
これは 2 つのフォールトトレラント戦略で共有されるプリミティブです：

- **Replica**（サブシステム A）—— 共有状態への並行書き込みを調整する。
- **Leader/Follower**（サブシステム B）—— ロックをリーダーのリースとして使う。

## トレイト

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

## バックエンド

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

`key` ごとに 1 つのロックファイルがルートディレクトリ配下に作成されます。
非ブロッキングの排他ロックとして `flock(LOCK_EX | LOCK_NB)` を使います。
別のプロセスがロックを保持している場合は `LockError::Contended` を返します。

> **Unix のみ。** `FileLock` は POSIX `flock` を使用し、Unix ターゲット
> （Linux、macOS、BSD）でのみ利用可能です。

### `LeaseLock`（TTL 付きファイルリース）—— feature `lease`

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

アトミックな一時ファイルのリネーム（CAS）でリースを取得します。バックグラウンド
スレッドが TTL/3 の間隔で更新します。クラッシュ時には、リースファイルの
`expires_at_ms` タイムスタンプにより、次の取得者が引き継げます。

### `PgLock`（PostgreSQL アドバイザリロック）—— feature `pg-lock`

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

`key` 文字列を FNV-1a ハッシュで派生させた bigint キーに対して、セッションレベルの
`pg_try_advisory_lock` / `pg_advisory_unlock` を使います。

## どれを使うべきか

| シナリオ | バックエンド |
| --- | --- |
| 単一ホスト、ロック保持者がクラッシュしない | `FileLock` |
| 単一ホスト、ロック保持者がクラッシュする可能性あり | `LeaseLock` |
| 複数ホスト、共有 Postgres | `PgLock` |
| 複数ホスト、共有 DB なし | 外部（etcd、Consul） |
