# Быстрый старт

## Добавление malkuth в ваш проект

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

## Минимальный сервер с плавной остановкой и пробами

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

## Что вы получаете

| Endpoint | Назначение |
| --- | --- |
| `GET /healthz` | Liveness — «процесс жив» (pid, время работы, версия) |
| `GET /readyz` | Readiness — «может обслуживать» (возвращает 503 во время drain) |

| Сигнал | Поведение |
| --- | --- |
| `SIGINT` / `SIGTERM` | Плавный drain (завершить текущие запросы, затем выйти) |
| `SIGHUP` | Горячая перезагрузка (**не выходит** — сервер продолжает обслуживать) |
| `SIGQUIT` | Немедленный выход (только в экстренных случаях) |

## Feature-флаги

| Компонент | Что включает |
| --- | --- |
| `socket-activation` | Наследование fd слушателя от systemd (перезапуск без простоев) |
| `file-lock` | Бэкенд `CoordinationLock` на базе `flock` POSIX |
| `lease` | Блокировка файла на основе lease с автоматическим истечением по TTL при сбое |
| `replica` | Трейт `InstanceRegistry` для реплик с балансировкой нагрузки |
| `leader-follower` | Трейт `LeaderElector` для active-passive HA |

Все компоненты подключаются опционально; стандартная сборка не содержит unsafe-кода и зависит только от tokio + axum.
