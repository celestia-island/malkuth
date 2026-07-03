# Verrous de coordination

## L'abstraction

`CoordinationLock` est un trait enfichable pour l'exclusion mutuelle entre
processus. C'est la primitive partagée des deux stratégies de tolérance aux
pannes :

- **Replica** (Sous-système A) — coordonner les écritures concurrentes vers un
  état partagé.
- **Leader/Follower** (Sous-système B) — utiliser le verrou comme bail de leader.

## Le trait

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

Un fichier de verrou par `key`, créé sous le répertoire racine. Utilise
`flock(LOCK_EX | LOCK_NB)` pour des verrous exclusifs non bloquants. Si un autre
processus détient le verrou, renvoie `LockError::Contended`.

> **Unix uniquement.** `FileLock` utilise `flock` POSIX et n'est disponible que
> sur les cibles Unix (Linux, macOS, BSD).

### `LeaseLock` (bail de fichier avec TTL) — feature `lease`

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

Utilise un renommage atomique de fichier temporaire (CAS) pour réclamer le bail.
Un thread d'arrière-plan le renouvelle à intervalles de TTL/3. En cas de crash,
l'horodatage `expires_at_ms` du fichier de bail permet au prochain acquéreur de
prendre le relais.

### `PgLock` (verrou consultatif PostgreSQL) — feature `pg-lock`

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

Utilise `pg_try_advisory_lock` / `pg_advisory_unlock` au niveau session sur une
clé bigint dérivée de la chaîne `key` via un hachage FNV-1a.

## Lequel choisir

| Scénario | Backend |
| --- | --- |
| Hôte unique, le détenteur ne plantera pas | `FileLock` |
| Hôte unique, le détenteur peut planter | `LeaseLock` |
| Plusieurs hôtes, Postgres partagé | `PgLock` |
| Plusieurs hôtes, pas de DB partagée | Externe (etcd, Consul) |
