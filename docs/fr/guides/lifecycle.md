# Arrêt gracieux et vidange

## Le problème

La plupart des serveurs Rust ne captent que `ctrl_c` (SIGINT). Or `docker stop`,
`systemctl restart` et la terminaison des pods Kubernetes envoient **SIGTERM** —
ce qui contourne l'arrêt gracieux et tue les requêtes en cours après l'expiration
du délai de grâce.

## `DrainController`

`DrainController` conserve un drapeau de vidange partagé et permet à n'importe
quelle tâche de l'attendre. Il repose sur `tokio::sync::Notify` + atomiques.

```rust
use malkuth::{DrainController, ShutdownKind};

let ctrl = DrainController::new();
```

## Sémantique des signaux

`SignalExitSource` (feature `signals`) installe des gestionnaires canoniques :

| Signal | `ShutdownKind` | Vidange ? | Sortie ? |
| --- | --- | --- | --- |
| `SIGINT` / `SIGTERM` | `Graceful` | Oui | Oui |
| `SIGHUP` | `Reload` | Non | Non (continue de servir) |
| `SIGQUIT` | `Immediate` | Oui (vidange ignorée) | Oui |

`SIGHUP` ne déclenche **pas** de vidange — `wait_for_drain()` ne se résout pas
lors d'un rechargement. Utilisez `wait_for_signal()` si vous devez aussi observer
les rechargements.

## Vidange par programme

Déclenchez la vidange depuis l'intérieur du processus (par ex. via un RPC
`Lifecycle.Drain`) :

```rust
ctrl.begin_drain(ShutdownKind::Graceful); // → all wait_for_drain() callers wake
```

## Observer l'état de vidange

```rust
// Non-blocking check:
if ctrl.is_draining() {
    // refuse new work
}

// Which kind fired?
if let Some(kind) = ctrl.kind() {
    println!("shutdown kind: {kind:?}");
}

// Async wait (resolves on Graceful or Immediate, NOT on Reload):
let kind = ctrl.wait_for_drain().await;

// Async wait for any signal (including Reload):
let kind = ctrl.wait_for_signal().await;
```

## Utiliser `Supervised`

`Supervised` compose `DrainController` + une source de sortie + des hooks de
vidange en une seule boucle de service :

```rust
use malkuth::{Supervised, Router};
use malkuth::transport::TcpTransport;
use malkuth::Transport;
use std::sync::Arc;

let supervised = Supervised::new()
    .signals()                          // install OS signal handler
    .on_drain(MyDrainHook)              // run cleanup during shutdown
    .drain_budget(std::time::Duration::from_secs(30));

let ctrl = supervised.drain_controller();
let handler = Arc::new(
    Router::new().lifecycle(ctrl, None)
);

// Serve JSON-RPC until a signal fires, then run drain hooks:
supervised
    .serve_rpc(&TcpTransport, "tcp://0.0.0.0:8080", handler)
    .await?;
```

## `ShutdownKind`

```rust
pub enum ShutdownKind {
    Graceful,   // SIGINT / SIGTERM — drain, then exit 0
    Immediate,  // SIGQUIT — skip drain, exit fast
    Reload,     // SIGHUP — reload config, do NOT exit
}
```
