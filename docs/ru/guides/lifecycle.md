# Плавная остановка и drain

## Проблема

Большинство Rust-серверов перехватывают только `ctrl_c` (SIGINT). Однако
`docker stop`, `systemctl restart` и завершение pod'ов Kubernetes отправляют
**SIGTERM** — что обходит вашу плавную остановку и убивает выполняемые запросы.

## Решение: `DrainController`

`DrainController::install()` устанавливает канонические обработчики сигналов,
следуя соглашению nginx/Go:

| Сигнал | Значение | Выполнять drain? |
| --- | --- | --- |
| `SIGINT` / `SIGTERM` | Плавная остановка | Да |
| `SIGHUP` | Горячая перезагрузка конфигурации | Нет (сервер продолжает работать) |
| `SIGQUIT` | Немедленный выход | Да (пропустить drain) |

## Использование

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

## Интеграция с `axum::serve`

```rust
axum::serve(listener, app)
    .with_graceful_shutdown(async {
        ctrl.wait_for_drain().await;
    })
    .await?;
```

`wait_for_drain` завершается при `SIGINT`/`SIGTERM`/`SIGQUIT`, но **не** при
`SIGHUP`, поэтому перезагрузка не приводит к случайной остановке сервера.

## Наблюдение за состоянием drain

```rust
// Non-blocking check:
if ctrl.is_draining() {
    // refuse new work
}

// Sleep, but wake early if drain begins:
ctrl.sleep_or_drain(std::time::Duration::from_secs(30)).await;
```

## Программный drain

Вы также можете запустить drain изнутри процесса (например, через
административный RPC):

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
