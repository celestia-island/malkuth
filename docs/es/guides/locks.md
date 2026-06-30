# Bloqueos de coordinación

## La abstracción

`CoordinationLock` es un trait conectable para la exclusión mutua entre procesos.
Es la primitiva compartida de ambas estrategias de tolerancia a fallos:

- **Replica** (Subsistema A) — coordinar escrituras concurrentes al estado compartido.
- **Leader/Follower** (Subsistema B) — usar el bloqueo como el lease del líder.

## El trait

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

### `file-lock` (`flock` POSIX)

Habilita la característica `file-lock`:

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

Un archivo de bloqueo por `key`, creado bajo el directorio raíz. Usa `flock(LOCK_EX | LOCK_NB)`
para bloqueos exclusivos no bloqueantes. Si otro proceso mantiene el bloqueo, devuelve
`LockError::Contended`.

### `lease` (bloqueo de archivo con TTL)

Habilita la característica `lease` (implica `file-lock`):

```toml
malkuth = { features = ["lease"] }
```

La misma API que `file-lock`, pero si el poseedor del bloqueo se cae, el lease expira
tras el TTL y otro proceso puede adquirirlo. Útil para despliegues de un solo host donde
el poseedor del bloqueo podría morir sin liberarlo.

### `pg-lock` (bloqueo consultivo de PostgreSQL)

Planificado — aún no implementado. Usará `pg_advisory_lock` para la coordinación
distribuida entre múltiples hosts que comparten una instancia de Postgres.

## Cuándo usar cuál

| Escenario | Backend |
| --- | --- |
| Un solo host, el poseedor no se caerá | `file-lock` |
| Un solo host, el poseedor podría caerse | `lease` |
| Múltiples hosts, Postgres compartido | `pg-lock` (planificado) |
| Múltiples hosts, sin DB compartida | Externo (etcd, Consul) |
