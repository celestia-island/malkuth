# Arrêt progressif et vidange

## Le problème

La plupart des serveurs Rust ne captent que `ctrl_c` (SIGINT). Or `docker stop`,
`systemctl restart` et la terminaison des pods Kubernetes envoient **SIGTERM** —
ce qui contourne votre arrêt progressif et tue les requêtes en cours.

## La solution : `DrainController`

`DrainController::install()` met en place des gestionnaires de signaux canoniques
selon la convention nginx/Go :

| Signal | Signification | Vidange ? |
| --- | --- | --- |
| `SIGINT` / `SIGTERM` | Arrêt progressif | Oui |
| `SIGHUP` | Rechargement à chaud de la config | Non (le serveur continue de servir) |
| `SIGQUIT` | Sortie immédiate | Oui (vidange ignorée) |

## Utilisation

```rust
use malkuth::DrainController;

let ctrl = DrainController::install();

// Pass clones to whoever needs to observe drain:
// - the serve loop (to stop accepting)
// - the probe layer (to set the /readyz draining bit)
// - background tasks (to wind down)

// Block until a drain/immediate signal fires.
let kind = ctrl.wait_for_drain().await;
```

## Intégration dans `axum::serve`

```rust
axum::serve(listener, app)
    .with_graceful_shutdown(async {
        ctrl.wait_for_drain().await;
    })
    .await?;
```

`wait_for_drain` se résout sur `SIGINT`/`SIGTERM`/`SIGQUIT` mais **pas** sur
`SIGHUP`, afin qu'un rechargement n'arrête pas accidentellement le serveur.

## Observer l'état de vidange

```rust
// Non-blocking check:
if ctrl.is_draining() {
    // refuse new work
}

// Sleep, but wake early if drain begins:
ctrl.sleep_or_drain(std::time::Duration::from_secs(30)).await;
```

## Vidange programmée

Vous pouvez également déclencher la vidange depuis l'intérieur du processus
(par ex. un RPC d'administration) :

```rust
ctrl.begin_drain(malkuth::ShutdownKind::Graceful);
```

## `ShutdownKind`

```rust
pub enum ShutdownKind {
    Graceful,   // SIGINT / SIGTERM — drain, then exit 0
    Immediate,  // SIGQUIT — skip drain, exit fast
    Reload,     // SIGHUP — reload config, do NOT exit
}
```
