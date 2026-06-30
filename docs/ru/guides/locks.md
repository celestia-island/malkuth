# Блокировки координации

## Абстракция

`CoordinationLock` — это подключаемый трейт для взаимного исключения между процессами.
Это общий примитив для обеих стратегий отказоустойчивости:

- **Replica** (Подсистема A) — координация параллельных записей в общее состояние.
- **Leader/Follower** (Подсистема B) — использование блокировки как lease лидера.

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

### `file-lock` (POSIX `flock`)

Включите компонент `file-lock`:

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

Один файл блокировки на каждый `key`, создаваемый в корневом каталоге. Использует
`flock(LOCK_EX | LOCK_NB)` для неблокирующих эксклюзивных блокировок. Если другой процесс
удерживает блокировку, возвращается `LockError::Contended`.

### `lease` (файловая блокировка с TTL)

Включите компонент `lease` (подразумевает `file-lock`):

```toml
malkuth = { features = ["lease"] }
```

API такой же, как у `file-lock`, но если владелец блокировки падает, lease истекает
по истечении TTL, и другой процесс может её получить. Полезно для развёртываний на одном
хосте, где владелец блокировки может умереть, не освободив её.

### `pg-lock` (консультативная блокировка PostgreSQL)

Запланировано — пока не реализовано. Будет использовать `pg_advisory_lock` для
распределённой координации между несколькими хостами, использующими общий экземпляр Postgres.

## Когда что использовать

| Сценарий | Бэкенд |
| --- | --- |
| Один хост, владелец не упадёт | `file-lock` |
| Один хост, владелец может упасть | `lease` |
| Несколько хостов, общий Postgres | `pg-lock` (запланировано) |
| Несколько хостов, без общей БД | Внешний (etcd, Consul) |
