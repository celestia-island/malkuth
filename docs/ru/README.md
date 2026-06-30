<!-- markdownlint-disable MD033 MD041 MD036 -->
<div align="center">

<img src="../res/logo.webp" alt="Логотип Plana" width="200"/>

# Plana

**Инфраструктура для долго работающих программ, позволяющая им самостоятельно обновляться и балансировать нагрузку**

[![License](https://img.shields.io/badge/license-BSL--1.1-blue.svg)](../LICENSE)
[![Rust](https://img.shields.io/badge/rust-1.85%2B-orange.svg)](https://www.rust-lang.org/)
[![GitHub](https://img.shields.io/badge/github-celestia--island%2Fplana-blue.svg)](https://github.com/celestia-island/plana)

</div>
<!-- markdownlint-enable MD033 MD041 MD036 -->

**[English](../../README.md)** &bull; **[简体中文](../zhs/README.md)** &bull;
**[繁體中文](../zht/README.md)** &bull; **[日本語](../ja/README.md)** &bull;
**[한국어](../ko/README.md)** &bull; **[Français](../fr/README.md)** &bull;
**[Español](../es/README.md)** &bull; **[Русский](../ru/README.md)**

> **Версия 0.1.0** — Ранняя разработка. Независимый и самодостаточный;
> зависит только от tokio + axum.

Plana помогает автоматизированным, долго работающим программам — демонам,
агентам, серверам — безопасно выполнять две сложные задачи:

- **Самообновление** — развёртывание новой версии (или свежескомпилированной
  сборки) без потери выполняемой работы или соединений: rolling-обновления без
  простоя.
- **Балансировка нагрузки** — запуск нескольких экземпляров, которые разделяют
  работу и координируют состояние, где один может корректно завершить работу,
  пока другой перехватывает управление.

## Строительные блоки

- **Lifecycle** — единая семантика сигналов (`SIGTERM` / `SIGINT` = drain,
  `SIGHUP` = reload, `SIGQUIT` = immediate) через `DrainController`.
- **Probes** — разделённые `/healthz` (liveness) + `/readyz` (readiness с битом
  drain), чтобы балансировщики нагрузки и оркестраторы могли маршрутизировать и
  выводить узлы из эксплуатации.
- **Workers** — контролируемые ресурсы дочерних процессов, каждый из которых
  является границей изоляции сбоев, с политикой перезапуска в стиле OTP и
  ограничением частоты методом скользящего окна.
- **Listener handoff** — наследование слушателя через socket-activation с
  резервным обычным bind для перезапусков без простоя.
- **Coordination locks** — подключаемый trait `CoordinationLock`
  (`file-lock` / `pg-lock` / `lease`) для координации параллельных записей или
  выбора лидера.

## Быстрый старт

```toml
[dependencies]
plana = { git = "https://github.com/celestia-island/plana.git", branch = "dev" }
# features: socket-activation, file-lock, lease, pg-lock, replica, leader-follower
```

```rust
use plana::{acquire_listener, probe_router, ProbeState, DrainController};

#[tokio::main]
async fn main() -> std::io::Result<()> {
    // Listener handoff: socket activation, falls back to a plain bind.
    let listener = acquire_listener("0.0.0.0:8080").await?;

    // Probes + signal-aware drain.
    let probe = ProbeState::new(env!("CARGO_PKG_VERSION"));
    let ctrl = DrainController::install();

    let app = axum::Router::new()
        .merge(probe_router(probe)) // GET /healthz, GET /readyz
        .with_state(());

    axum::serve(listener, app)
        .with_graceful_shutdown(async {
            // Resolves on SIGINT / SIGTERM (drain) or SIGQUIT (immediate),
            // but NOT on SIGHUP (reload — the server keeps serving).
            ctrl.wait_for_drain().await;
        })
        .await?;
    Ok(())
}
```

## Флаги функций

| Возможность | Включает |
| --- | --- |
| `socket-activation` | наследование fd слушателя (socket activation) |
| `file-lock` | бэкенд `CoordinationLock` на основе POSIX `flock` |
| `lease` | файловая блокировка на основе аренды с автоистечением по TTL |
| `pg-lock` | бэкенд `pg_advisory_lock` PostgreSQL (в разработке) |
| `replica` | trait `InstanceRegistry` (балансировка нагрузки / rolling update) |
| `leader-follower` | trait `LeaderElector` (активно-пассивная HA) |

## Статус

Реализованы Lifecycle + probes, контролируемые workers, listener handoff и
trait coordination-lock с бэкендом `file-lock`. Бэкенды стратегий
`replica` / `leader-follower` являются контрактами trait с полностью
запланированными реализациями.

## Лицензия

Business Source License 1.1 (BSL-1.1); автоматически преобразуется на ваш выбор
в Apache-2.0 или MIT 1 января 2030 года. См. [LICENSE](../LICENSE).
