# Malkuth
<!-- markdownlint-disable MD033 MD041 MD036 -->
<div align="center">

<img src="../logo.webp" alt="Malkuth" width="200"/>


**Инфраструктура для долгоживущих программ с самообновлением и балансировкой нагрузки**

[![License](https://img.shields.io/badge/license-BSL--1.1-blue.svg)](LICENSE)
[![Rust](https://img.shields.io/badge/rust-1.85%2B-orange.svg)](https://www.rust-lang.org/)
[![GitHub](https://img.shields.io/badge/github-celestia--island%2Fmalkuth-blue.svg)](https://github.com/celestia-island/malkuth)

</div>
<!-- markdownlint-enable MD033 MD041 MD036 -->

**[English](../en/README.md)** &bull; **[简体中文](../zhs/README.md)** &bull;
**[繁體中文](../zht/README.md)** &bull; **[日本語](../ja/README.md)** &bull;
**[한국어](../ko/README.md)** &bull; **[Français](../fr/README.md)** &bull;
**[Español](../es/README.md)** &bull; **[Русский](README.md)**

> **Версия 0.1.0** — Ранняя стадия разработки. Независимый и самодостаточный;
> зависит только от tokio + axum.

Malkuth помогает автоматизированным долгоживущим программам — демонам,
агентам, серверам — безопасно выполнять две сложные задачи:

- **Самообновление** — выпустить новую версию (или свежесобранную сборку), не
  теряя выполняемую в данный момент работу и активные соединения: скользящее
  обновление без простоев.
- **Балансировка нагрузки** — запуск нескольких экземпляров, которые разделяют
  работу и координируют состояние, где один может корректно завершаться, пока
  другой принимает управление на себя.

## Базовые компоненты

- **Жизненный цикл** — единая семантика сигналов (`SIGTERM` / `SIGINT` = drain,
  `SIGHUP` = перезагрузка, `SIGQUIT` = немедленное завершение) через
  `DrainController`.
- **Пробы** — разделённые `/healthz` (liveness) + `/readyz` (readiness с битом
  drain), чтобы балансировщики нагрузки и оркестраторы могли маршрутизировать
  трафик и выводить узлы из эксплуатации.
- **Воркеры** — контролируемые ресурсы в виде дочерних процессов, каждый из
  которых служит границей изоляции сбоев, с политикой перезапуска в стиле OTP и
  ограничением скорости на скользящем окне.
- **Передача слушателя** — наследование слушателя через socket activation с
  резервным вариантом обычной привязки (plain-bind) для перезапусков без
  простоев.
- **Координационные замки** — подключаемый трейт `CoordinationLock`
  (`file-lock` / `pg-lock` / `lease`) для координации параллельных записей или
  выборов лидера.

## Быстрый старт

```toml
[dependencies]
malkuth = { git = "https://github.com/celestia-island/malkuth.git", branch = "dev" }
# features: socket-activation, file-lock, lease, pg-lock, replica, leader-follower
```

```rust
use malkuth::{acquire_listener, probe_router, ProbeState, DrainController};

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

## Feature-флаги

| Компонент | Что включает |
| --- | --- |
| `socket-activation` | наследование fd слушателя (socket activation) |
| `file-lock` | бэкенд `CoordinationLock` на базе POSIX `flock` |
| `lease` | блокировка файла на основе аренды (lease) с автоматическим истечением по TTL |
| `pg-lock` | бэкенд PostgreSQL `pg_advisory_lock` (поэтапно) |
| `replica` | трейт `InstanceRegistry` (балансировка нагрузки / скользящее обновление) |
| `leader-follower` | трейт `LeaderElector` (active-passive HA) |

## Статус

Жизненный цикл + пробы, контролируемые воркеры, передача слушателя и трейт
координационного замка с бэкендом `file-lock` реализованы. Стратегийные
бэкенды `replica` / `leader-follower` представляют собой контракты трейтов с
полными реализациями, запланированными поэтапно. Дизайн см. в
[docs/design/](design/).

## Лицензия

Business Source License 1.1 (BSL-1.1); автоматически конвертируется, на ваш
выбор, в Apache-2.0 или MIT 2030-01-01. См. [LICENSE](../../LICENSE).
