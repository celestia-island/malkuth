# Démarrage rapide

## Ajouter malkuth à votre projet

```toml
[dependencies]
malkuth = { git = "https://github.com/celestia-island/malkuth.git", branch = "dev" }
# Optional features:
#   socket-activation  — inherit a listener fd from systemd
#   file-lock          — POSIX flock coordination-lock backend
#   lease              — lease-based file lock with TTL auto-expiry
#   replica            — InstanceRegistry trait (load-balancing)
#   leader-follower    — LeaderElector trait (active-passive HA)
```

## Serveur minimal avec arrêt progressif et sondes

```rust
use malkuth::{acquire_listener, probe_router, ProbeState, DrainController};

#[tokio::main]
async fn main() -> std::io::Result<()> {
    // 1. Acquire a listener — prefers systemd socket activation,
    //    falls back to binding the given address.
    let listener = acquire_listener("0.0.0.0:8080").await?;

    // 2. Create probe state and install the drain controller.
    let probe = ProbeState::new(env!("CARGO_PKG_VERSION"));
    let ctrl = DrainController::install();

    // 3. Build your router, merging the probe routes.
    let app = axum::Router::new()
        .route("/", axum::routing::get(|| async { "hello" }))
        .merge(probe_router(probe))
        .with_state(());

    // 4. Serve with graceful shutdown: SIGINT/SIGTERM trigger drain,
    //    SIGQUIT forces immediate exit, SIGHUP reloads (keeps serving).
    axum::serve(listener, app)
        .with_graceful_shutdown(async {
            ctrl.wait_for_drain().await;
        })
        .await?;
    Ok(())
}
```

## Ce que vous obtenez

| Endpoint | Rôle |
| --- | --- |
| `GET /healthz` | Vitalité — « le processus est en vie » (pid, durée de fonctionnement, version) |
| `GET /readyz` | Préparation — « peut servir » (renvoie 503 pendant la vidange) |

| Signal | Comportement |
| --- | --- |
| `SIGINT` / `SIGTERM` | Vidange progressive (terminer le travail en cours, puis sortir) |
| `SIGHUP` | Rechargement à chaud (**ne sort pas** — le serveur continue de servir) |
| `SIGQUIT` | Sortie immédiate (urgence uniquement) |

## Fonctionnalités optionnelles

| Fonctionnalité | Ce qu'elle active |
| --- | --- |
| `socket-activation` | Hériter d'un fd de listener depuis systemd (redémarrage sans interruption) |
| `file-lock` | Backend `CoordinationLock` basé sur `flock` POSIX |
| `lease` | Verrou fichier à bail avec expiration automatique par TTL en cas de crash |
| `replica` | Trait `InstanceRegistry` pour les réplicas à charge équilibrée |
| `leader-follower` | Trait `LeaderElector` pour la HA actif-passif |

Toutes les fonctionnalités sont opt-in ; la compilation par défaut ne contient aucun code unsafe et ne dépend que de tokio + axum.
