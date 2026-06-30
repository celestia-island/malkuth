# Пробы здоровья

## Разделение проб: `/healthz` и `/readyz`

Malkuth следует соглашению Kubernetes о **двух отдельных эндпоинтах проб**:

- **`GET /healthz`** —— *Liveness*: "Жив ли процесс?" В случае сбоя оркестратор
  **перезапускает** экземпляр.
- **`GET /readyz`** —— *Readiness*: "Может ли этот экземпляр обслуживать трафик прямо
  сейчас?" В случае сбоя оркестратор **перестаёт направлять трафик**, но не
  перезапускает.

Это различие важно при скользящем обновлении: экземпляр в состоянии drain *жив*
(healthz = 200), но *не готов* (readyz = 503).

## Настройка

```rust
use malkuth::{ProbeState, probe_router};

let probe = ProbeState::new(env!("CARGO_PKG_VERSION"));

// Register dependencies that affect readiness:
probe.add_dependency("database", || {
    // Return true if the DB connection is healthy.
    // This is a sync closure — keep it cheap (read an atomic, ping a cached conn).
    true
}).await;

// Merge the probe routes into your app:
let app = axum::Router::new()
    .merge(probe_router(probe));
```

## Формат ответов

### `/healthz` (всегда 200, если процесс может ответить)

```json
{
  "alive": true,
  "pid": 12345,
  "uptime_secs": 3600,
  "version": "0.1.0"
}
```

### `/readyz` (503, когда не готов)

```json
{
  "ready": true,
  "draining": false,
  "dependencies": [
    { "name": "database", "ok": true }
  ],
  "generation": 2
}
```

При drain или неработоспособности зависимости `ready` равен `false`, а HTTP-статус —
`503 Service Unavailable`.

## Подключение бита drain

Во время плавной остановки установите флаг drain, чтобы `/readyz` начал возвращать
503:

```rust
let probe = ProbeState::new(env!("CARGO_PKG_VERSION"));
let ctrl = DrainController::install();

// In your shutdown sequence:
tokio::spawn({
    let probe = probe.clone();
    let ctrl = ctrl.clone();
    async move {
        // Wait until drain begins, then flip the bit.
        ctrl.wait_for_drain().await;
        probe.set_draining(true).await;
    }
});
```

Теперь балансировщик нагрузки видит, как `/readyz` переходит в 503, и прекращает
отправку нового трафика **до того**, как процесс завершится — основа скользящих
обновлений без простоев.

## Поколение развёртывания

Отслеживайте, к какому поколению развёртывания принадлежит этот экземпляр:

```rust
probe.set_generation(Some(2)).await; // generation 2 of a rolling update
```

Это значение включается в ответ `/readyz` для наблюдаемости и оркестрации.
