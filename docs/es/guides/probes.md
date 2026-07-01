# Sondeos de salud

## El trait `ProbeSink`

Malkuth separa **el estado del sondeo** de **cómo se expone**. El trait
`ProbeSink` define dos consultas:

```rust
#[async_trait]
pub trait ProbeSink: Send + Sync {
    async fn ready(&self) -> ReadyStatus;
    async fn health(&self) -> HealthStatus;
}
```

Cualquier tipo que implemente `ProbeSink` puede ser consultado vía JSON-RPC o
HTTP.

## `ProbeState` — la implementación integrada

`ProbeState` mantiene la información de versión, una bandera de estado de
drenaje, un contador de generación y una lista de comprobaciones de
dependencias:

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

## Exposición por JSON-RPC (principal)

`Router::lifecycle(ctrl, Some(probe))` registra los métodos estándar y consulta
el `ProbeSink` en cada llamada:

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

Cuando `draining` es `true` o cualquier dependencia es `ok: false`, `ready` es
`false`.

## Exposición por HTTP (opcional, feature `probes`)

Para sondeos HTTP de estilo Kubernetes o balanceadores de carga externos que
esperan HTTP, habilita la feature `probes` para obtener rutas axum:

```rust
use malkuth::{ProbeState, probe_router};

let probe = ProbeState::new(env!("CARGO_PKG_VERSION"));
probe.add_dependency("database", || { true });

let app = axum::Router::new()
    .merge(probe_router(probe));   // GET /healthz + GET /readyz
```

| Endpoint | Devuelve | Estado HTTP |
| --- | --- | --- |
| `GET /healthz` | `HealthStatus` | Siempre 200 |
| `GET /readyz` | `ReadyStatus` | 200 si está listo, 503 si drenando / dependencia caída |

Las formas de respuesta son idénticas a los métodos JSON-RPC — `ProbeState`
implementa `ProbeSink`, por lo que ambas rutas consultan el mismo estado
subyacente.

## Conectar el drenaje a los sondeos

Durante el apagado gracioso, define el estado de drenaje para que
`Lifecycle.Status` (y `/readyz`) lo reflejen:

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

Así el orquestador ve cómo la preparación cambia a `false` **antes** de que el
proceso termine — el núcleo de las actualizaciones progresivas sin tiempo de
inactividad.
