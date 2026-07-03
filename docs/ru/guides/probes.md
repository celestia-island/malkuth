# Пробы здоровья

## Трейт `ProbeSink`

Malkuth разделяет **состояние пробы** и **способ её представления**. Трейт
`ProbeSink` определяет два запроса:

```rust
#[async_trait]
pub trait ProbeSink: Send + Sync {
    async fn ready(&self) -> ReadyStatus;
    async fn health(&self) -> HealthStatus;
}
```

Любой тип, реализующий `ProbeSink`, можно опрашивать через JSON-RPC или HTTP.

## `ProbeState` — встроенная реализация

`ProbeState` хранит информацию о версии, флаг состояния дрейна, счётчик поколения
и список проверок зависимостей:

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

## Представление через JSON-RPC (основной путь)

`Router::lifecycle(ctrl, Some(probe))` регистрирует стандартные методы и при
каждом вызове опрашивает `ProbeSink`:

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

Когда `draining` равно `true` или любая зависимость имеет `ok: false`, значение
`ready` — `false`.

## Представление через HTTP (опционально, feature `probes`)

Для HTTP-проб в стиле Kubernetes или внешних балансировщиков, ожидающих HTTP,
включите feature `probes`, чтобы получить axum-маршруты:

```rust
use malkuth::{ProbeState, probe_router};

let probe = ProbeState::new(env!("CARGO_PKG_VERSION"));
probe.add_dependency("database", || { true });

let app = axum::Router::new()
    .merge(probe_router(probe));   // GET /healthz + GET /readyz
```

| Endpoint | Возвращает | HTTP-статус |
| --- | --- | --- |
| `GET /healthz` | `HealthStatus` | Всегда 200 |
| `GET /readyz` | `ReadyStatus` | 200 если готов, 503 при дрейне / падении зависимости |

Форма ответа идентична методам JSON-RPC — `ProbeState` реализует `ProbeSink`,
поэтому оба пути опрашивают одно и то же нижележащее состояние.

## Подключение дрейна к пробам

Во время мягкого завершения установите состояние дрейна, чтобы
`Lifecycle.Status` (и `/readyz`) его отражали:

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

Теперь оркестратор видит, как готовность переключается в `false` **до** того,
как процесс завершится — ядро бесшовных плавающих обновлений.
