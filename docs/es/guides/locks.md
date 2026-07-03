# Cerrojos de coordinación

## La abstracción

`CoordinationLock` es un trait enchufable para la exclusión mutua entre procesos.
Es la primitiva compartida de las dos estrategias de tolerancia a fallos:

- **Replica** (Subsistema A) — coordinar escrituras concurrentes a estado
  compartido.
- **Leader/Follower** (Subsistema B) — usar el cerrojo como el lease del líder.

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

### `FileLock` (`flock` POSIX) — feature `file-lock`

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

Un archivo de cerrojo por cada `key`, creado bajo el directorio raíz. Usa
`flock(LOCK_EX | LOCK_NB)` para cerrojos exclusivos no bloqueantes. Si otro
proceso retiene el cerrojo, devuelve `LockError::Contended`.

> **Solo Unix.** `FileLock` usa `flock` POSIX y solo está disponible en destinos
> Unix (Linux, macOS, BSD).

### `LeaseLock` (lease de archivo con TTL) — feature `lease`

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

Usa renombrado atómico de archivo temporal (CAS) para reclamar el lease. Un hilo
en segundo plano lo renueva a intervalos de TTL/3. Si hay caída, el sello de
tiempo `expires_at_ms` del archivo de lease permite al próximo adquirente tomar
el relevo.

### `PgLock` (cerrojo consultivo de PostgreSQL) — feature `pg-lock`

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

Usa `pg_try_advisory_lock` / `pg_advisory_unlock` a nivel sesión sobre una clave
bigint derivada de la cadena `key` mediante un hash FNV-1a.

## Cuál usar cuándo

| Escenario | Backend |
| --- | --- |
| Host único, el titular no caerá | `FileLock` |
| Host único, el titular podría caer | `LeaseLock` |
| Múltiples hosts, Postgres compartido | `PgLock` |
| Múltiples hosts, sin DB compartida | Externo (etcd, Consul) |
