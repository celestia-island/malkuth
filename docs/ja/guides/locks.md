# 調整ロック

## 抽象化

`CoordinationLock` は、プロセス間の相互排他用のプラグイン可能な trait です。
これは2つのフォールトトレランス戦略の共通プリミティブです：

- **Replica**（サブシステム A）—— 共有状態への並行書き込みを調整する。
- **Leader/Follower**（サブシステム B）—— ロックをリーダーリースとして使用する。

## この trait

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

### `file-lock`（POSIX `flock`）

`file-lock` フィーチャーを有効にします：

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

`key` ごとに1つのロックファイルが、ルートディレクトリ配下に作成されます。非ブロッキングの
排他ロックには `flock(LOCK_EX | LOCK_NB)` を使用します。別のプロセスがロックを保持している場合は、
`LockError::Contended` を返します。

### `lease`（TTL 付きファイルロック）

`lease` フィーチャーを有効にします（`file-lock` を暗黙的に含みます）：

```toml
malkuth = { features = ["lease"] }
```

API は `file-lock` と同じですが、ロック保持者がクラッシュした場合、TTL の経過後にリースが
失効し、別のプロセスが取得できるようになります。ロック保持者が解放せずに死ぬ可能性のある
単一ホストデプロイメントに有用です。

### `pg-lock`（PostgreSQL アドバイザリロック）

計画中 —— まだ実装されていません。1つの Postgres インスタンスを共有する複数ホスト間での
分散調整に `pg_advisory_lock` を使用する予定です。

## どれをいつ使うか

| シナリオ | バックエンド |
| --- | --- |
| 単一ホスト、ロック保持者はクラッシュしない | `file-lock` |
| 単一ホスト、ロック保持者がクラッシュする可能性あり | `lease` |
| 複数ホスト、共有 Postgres | `pg-lock`（計画中） |
| 複数ホスト、共有 DB なし | 外部（etcd、Consul） |
