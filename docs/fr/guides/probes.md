# Sondes de santé

## Le trait `ProbeSink`

Malkuth sépare **l'état de sonde** de **la façon dont il est exposé**. Le trait
`ProbeSink` définit deux requêtes :

```rust
#[async_trait]
pub trait ProbeSink: Send + Sync {
    async fn ready(&self) -> ReadyStatus;
    async fn health(&self) -> HealthStatus;
}
```

Tout type qui implémente `ProbeSink` peut être interrogé via JSON-RPC ou HTTP.

## `ProbeState` — l'implémentation intégrée

`ProbeState` contient les informations de version, un drapeau d'état de vidange,
un compteur de génération et une liste de vérifications de dépendances :

```rust
use malkuth::{ProbeState, DrainState};

let probe = ProbeState::new(env!("CARGO_PKG_VERSION"));

// Register a dependency that affects readiness.
// The closure is synchronous — keep it cheap (read an atomic, ping a cached conn).
probe.add_dependency("database", || { /* return true if healthy */ true });

// Flip the drain bit during shutdown:
probe.set_drain_state(DrainState::Draining);

// Record the deployment generation (visible in the status response):
probe.set_generation(Some(2));
```

## Exposition via JSON-RPC (principale)

`Router::lifecycle(ctrl, Some(probe))` enregistre les méthodes standard et
interroge le `ProbeSink` à chaque appel :

```rust
use std::sync::Arc;
use malkuth::{ProbeState, Router, Supervised};
use malkuth::transport::TcpTransport;
use malkuth::Transport;

let supervised = Supervised::new().signals();
let ctrl = supervised.drain_controller();
let probe = Arc::new(ProbeState::new("0.1.0"));

let handler = Arc::new(
    Router::new()
        .lifecycle(ctrl, Some(probe.clone()))
        .route("ping", |_| Box::pin(async { Ok(serde_json::json!("pong")) })),
);

supervised.serve_rpc(&TcpTransport, "tcp://0.0.0.0:8080", handler).await?;
```

### `Lifecycle.Health` → `HealthStatus`

```json
// Request: { "jsonrpc": "2.0", "id": 1, "method": "Lifecycle.Health", "params": {} }
// Response:
{ "alive": true, "pid": 12345, "uptime_secs": 360, "version": "0.1.0" }
```

### `Lifecycle.Status` → `ReadyStatus`

```json
// Request: { "jsonrpc": "2.0", "id": 2, "method": "Lifecycle.Status", "params": {} }
// Response:
{
  "ready": true,
  "draining": false,
  "dependencies": [{ "name": "database", "ok": true }],
  "generation": 2
}
```

Quand `draining` vaut `true` ou qu'une dépendance est `ok: false`, `ready` vaut
`false`.

## Exposition via HTTP (optionnelle, feature `probes`)

Pour des sondes HTTP de style Kubernetes ou des équilibreurs de charge externes
qui attendent du HTTP, activez la feature `probes` pour obtenir des routes axum :

```rust
use malkuth::{ProbeState, probe_router};

let probe = ProbeState::new(env!("CARGO_PKG_VERSION"));
probe.add_dependency("database", || { true });

let app = axum::Router::new()
    .merge(probe_router(probe));   // GET /healthz + GET /readyz
```

| Endpoint | Renvoie | Statut HTTP |
| --- | --- | --- |
| `GET /healthz` | `HealthStatus` | Toujours 200 |
| `GET /readyz` | `ReadyStatus` | 200 si prêt, 503 si vidange / dépendance en panne |

Les formes de réponse sont identiques à celles des méthodes JSON-RPC —
`ProbeState` implémente `ProbeSink`, donc les deux chemins interrogent le même
état sous-jacent.

## Câbler la vidange aux sondes

Pendant l'arrêt gracieux, définissez l'état de vidange pour que
`Lifecycle.Status` (et `/readyz`) le reflètent :

```rust
use malkuth::{DrainController, DrainState, ShutdownKind};

let ctrl = DrainController::new();
let probe = ProbeState::new("0.1.0");

tokio::spawn({
    let probe = probe.clone();
    let ctrl = ctrl.clone();
    async move {
        ctrl.wait_for_drain().await;
        probe.set_drain_state(DrainState::Draining);
    }
});
```

Désormais, l'orchestrateur voit la disponibilité basculer à `false` **avant** que
le processus ne se termine — c'est le cœur des mises à jour progressives sans
indisponibilité.
