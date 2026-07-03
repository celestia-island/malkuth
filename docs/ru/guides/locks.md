# Координационные блокировки

## Абстракция

`CoordinationLock` — это подключаемый трейт для взаимного исключения между
процессами. Это общий примитив для обеих стратегий отказоустойчивости:

- **Replica** (Подсистема A) — координировать параллельные записи в разделяемое
  состояние.
- **Leader/Follower** (Подсистема B) — использовать блокировку как лидерский
  lease.

## Трейт

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

## Бэкенды

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

Один файл блокировки на каждый `key`, создаваемый в корневом каталоге. Использует
`flock(LOCK_EX | LOCK_NB)` для неблокирующих эксклюзивных блокировок. Если другой
процесс удерживает блокировку, возвращает `LockError::Contended`.

> **Только Unix.** `FileLock` использует POSIX `flock` и доступен только на
> Unix-таргетах (Linux, macOS, BSD).

### `LeaseLock` (файловый lease с TTL) — feature `lease`

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

Использует атомарное переименование временного файла (CAS) для захвата lease.
Фоновый поток обновляет его с интервалом TTL/3. При падении временная метка
`expires_at_ms` файла lease позволяет следующему захватчику перехватить его.

### `PgLock` (консультативная блокировка PostgreSQL) — feature `pg-lock`

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

Использует сессионные `pg_try_advisory_lock` / `pg_advisory_unlock` на ключе
типа bigint, полученном из строки `key` через хеш FNV-1a.

## Что когда использовать

| Сценарий | Бэкенд |
| --- | --- |
| Один хост, держатель не упадёт | `FileLock` |
| Один хост, держатель может упасть | `LeaseLock` |
| Несколько хостов, общий Postgres | `PgLock` |
| Несколько хостов, без общей БД | Внешний (etcd, Consul) |
