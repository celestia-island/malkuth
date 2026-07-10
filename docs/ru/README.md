<p align="center"><img src="https://raw.githubusercontent.com/celestia-island/malkuth/master/docs/logo.webp" alt="Malkuth" width="240" /></p>

<h1 align="center">Malkuth</h1>

<p align="center"><strong>Компонуемый набор инструментов для контроля служб на Rust</strong></p>

<div align="center">

[![License: SySL-1.0](https://img.shields.io/badge/License-SySL--1.0-blue.svg)](https://sysl.celestia.world)
[![GitHub](https://img.shields.io/badge/github-celestia--island%2Fmalkuth-blue.svg)](https://github.com/celestia-island/malkuth)
[![Checks](https://img.shields.io/github/actions/workflow/status/celestia-island/malkuth/checks.yml)](https://github.com/celestia-island/malkuth/actions/workflows/checks.yml)
[![Docs](https://img.shields.io/badge/docs-malkuth.docs.celestia.world-blue)](https://malkuth.docs.celestia.world)
[![docs.rs](https://docs.rs/malkuth/badge.svg)](https://docs.rs/malkuth)

</div>

<div align="center">

[English](../en/README.md) · [简体中文](../zhs/README.md) · [繁體中文](../zht/README.md) · [日本語](../ja/README.md) · [한국어](../ko/README.md) · [Français](../fr/README.md) · [Español](../es/README.md) · **Русский** · [العربية](../ar/README.md)

</div>

Malkuth помогает автоматизированным долгоживущим программам решать четыре
сложные задачи:

1. **Подключаемый транспорт** — JSON-RPC поверх локальной петли TCP, удалённого
   **WebSocket** или локального **IPC** (Unix-сокеты / именованные каналы через
   [`interprocess`](https://crates.io/crates/interprocess)). Один трейт
   `Transport`, диспетчеризуемый по схеме URL.
2. **Контролируемые воркеры** — порождать процесс, отслеживать его состояние, перезапускать при сбое, сливать соединения перед остановкой.
3. **Опциональные, перехватываемые возможности** — источник выхода, пробы, хуки
   пульса и слива — это *трейты*. Используйте умолчания (выход по сигналу ОС,
   пробы axum, контролируемые воркеры) или предоставьте свои (например,
   запускайте слив по встроенной команде «stop», полученной вашим сервером).
   Полностью укомплектованный оркестратор `Supervised` связывает их воедино.
4. **CLI-наблюдатель** — `malkuth -- <cmd>` оборачивает программу наблюдением
   за файлами, пулом подов и закреплённым обратным прокси уровня L4.

## Как CLI

```
malkuth [--watch PATH]... [--proxy PUBLIC:LO-HI] [--pod-count N] -- <cmd> [args...]
```

Запустите 5 параллельных копий вашего сервера (каждая слушает переменную
окружения `PORT` → они самостоятельно занимают порты 3001–3005), с закреплённым
прокси на порту 3000:

```bash
malkuth --watch ./src --watch ./res \
        --proxy 3000:3000-3999 --pod-count 5 \
        -- cargo run
```

Прокси направляет каждый **клиентский IP** к фиксированному бэкенду через
консистентное хеширование, благодаря чему клиент продолжает попадать на один и
тот же под, пока тот не перезапустится или не будет удалён при
масштабировании — основа для серого релиза / плавного перезапуска. При
изменении файла он сливает и перезапускает поды по одному.

## Как библиотека

```toml
[dependencies]
malkuth = "0.1"
# features: tcp (default) | ws | ipc | signals (default) | worker | probes |
#           file-lock | lease | pg-lock | replica | leader-follower | schema | cli
```

```rust
use std::sync::Arc;
use malkuth::{Client, Router, Server, Supervised, Transport};
use malkuth::transport::TcpTransport;
use serde_json::json;

#[tokio::main]
async fn main() -> std::io::Result<()> {
    // Bind once; build a router with the standard lifecycle RPC + your methods.
    let lis = TcpTransport.listen("tcp://127.0.0.1:0").await?;
    let supervised = Supervised::new().signals();           // OS-signal exit source
    let ctrl = supervised.drain_controller();
    let handler = Arc::new(
        Router::new()
            .lifecycle(ctrl, None)                          // Lifecycle.Drain/Status/...
            .route("ping", |_| Box::pin(async { Ok(json!("pong")) })),
    );
    // Race the server against the exit source, then run drain hooks.
    supervised.serve_rpc_listener(lis, handler).await
}
```

Нужен слив, запускаемый вашей собственной логикой вместо сигналов?
Реализуйте `malkuth::ExitSource` и передайте его через `.exit(...)`. Нужна
координация на базе Postgres? Функция `pg-lock` предоставляет бэкенд
`CoordinationLock`.

## Флаги функций

| Функция | Включает |
| --- | --- |
| `tcp` *(по умолчанию)* | JSON-RPC поверх локального/удалённого TCP (`tokio::net`) |
| `ws` | JSON-RPC поверх WebSocket (`tokio-tungstenite`) |
| `ipc` | JSON-RPC поверх локального IPC (`interprocess`) |
| `signals` *(по умолчанию)* | Стандартный `ExitSource` по сигналам ОС (`tokio::signal`) |
| `worker` | Контролируемые дочерние процессы-воркеры (`tokio::process`) |
| `probes` | Роутер axum `/healthz` + `/readyz` |
| `file-lock` | Бэкенд `CoordinationLock` на POSIX `flock` (unix) |
| `lease` | `CoordinationLock` на файловой аренде с автоистечением TTL (устойчив к сбоям) |
| `pg-lock` | Бэкенд PostgreSQL `pg_advisory_lock` (`tokio-postgres`) |
| `replica` | `InstanceRegistry` в памяти |
| `leader-follower` | `LeaseLeaderElector` (поверх бэкенда аренды) |
| `schema` | Реализации `schemars::JsonSchema` для типов передачи данных |
| `cli` | Бинарный наблюдатель `malkuth` (пул подов + закреплённый прокси) |

## Статус

Слои 1–3 (жизненный цикл/слив, пробы, передача слушателя) и ядро JSON-RPC
(кодек + сервер/клиент + транспорты tcp/ws/ipc) реализованы и протестированы
end-to-end. Пул подов CLI + закреплённый обратный прокси работают (проверено
e2e). Все три бэкенда `CoordinationLock` (`file-lock`, `lease`, `pg-lock`) и
`LeaseLeaderElector` в режиме leader-follower реализованы. См.
[проектирование](../en/design/).

## MCP-сервер

Соберите malkuth с feature `mcp` и запустите stdio-сервер — он предоставляет набор инструментов супервизора AI-ассистентам программиста по протоколу Model Context Protocol:

```bash
malkuth mcp
```

Сервер предоставляет два инструмента: `malkuth_supervise` (запускает набор воркеров под супервизором с политиками перезапуска + скользящим ограничением скорости; блокируется до их завершения или срабатывания тайм-аута, затем возвращает финальный снимок состояния) и `malkuth_probe` (проверка HTTP healthz / readyz по URL сервиса). Подключите его к MCP-клиенту:

```json
{
  "mcpServers": {
    "malkuth": { "command": "malkuth", "args": ["mcp"] }
  }
}
```

Feature `mcp` неявно включает `worker` + `schema`; она добавляет `rmcp` и клиент `reqwest` для инструмента проверки.

## Лицензия

SySL-1.0（Synthetic Source License）。См. [LICENSE](https://sysl.celestia.world)。
