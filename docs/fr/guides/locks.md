# Verrous de coordination

## L'abstraction

`CoordinationLock` est un trait enfichable pour l'exclusion mutuelle entre processus.
C'est la primitive partagée des deux stratégies de tolérance aux pannes :

- **Replica** (Sous-système A) — coordonner les écritures concurrentes vers l'état partagé.
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

### `file-lock` (`flock` POSIX)

Activez la fonctionnalité `file-lock` :

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

Un fichier de verrou par `key`, créé sous le répertoire racine. Utilise `flock(LOCK_EX | LOCK_NB)`
pour des verrous exclusifs non-bloquants. Si un autre processus détient le verrou, renvoie
`LockError::Contended`.

### `lease` (verrou fichier avec TTL)

Activez la fonctionnalité `lease` (implique `file-lock`) :

```toml
malkuth = { features = ["lease"] }
```

Même API que `file-lock`, mais si le détenteur du verrou plante, le bail expire après le TTL
et un autre processus peut l'acquérir. Utile pour les déploiements mono-hôte où le détenteur
du verrou risque de mourir sans le relâcher.

### `pg-lock` (verrou consultatif PostgreSQL)

Planifié — pas encore implémenté. Utilisera `pg_advisory_lock` pour la coordination
distribuée entre plusieurs hôtes partageant une instance Postgres.

## Quand utiliser lequel

| Scénario | Backend |
| --- | --- |
| Hôte unique, le détenteur ne plantera pas | `file-lock` |
| Hôte unique, le détenteur peut planter | `lease` |
| Plusieurs hôtes, Postgres partagé | `pg-lock` (planifié) |
| Plusieurs hôtes, pas de DB partagée | Externe (etcd, Consul) |
