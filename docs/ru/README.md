# Malkuth
<!-- markdownlint-disable MD033 MD041 MD036 -->
<div align="center">

<img src="../logo.webp" alt="Malkuth" width="200"/>

**Компонуемый набор инструментов для контроля служб на Rust — JSON-RPC поверх подключаемых транспортов, контролируемые воркеры, координационные блокировки и выбор лидера, а также CLI-наблюдатель (watchdog).**

[![License](https://img.shields.io/badge/license-SySL%201.0-blue)](../../LICENSE)
[![Rust](https://img.shields.io/badge/rust-1.85%2B-orange.svg)](https://www.rust-lang.org/)
[![GitHub](https://img.shields.io/badge/github-celestia--island%2Fmalkuth-blue.svg)](https://github.com/celestia-island/malkuth)

</div>
<!-- markdownlint-enable MD033 MD041 MD036 -->

<!-- language switcher is available in the bottom-right corner -->

> **Версия 0.1.0** — Одиночный крейт, **на базе tokio**. CLI оборачивает
> *любую* программу (даже ту, что не использует библиотеку) пулом подов и
> закрепляющимся обратным прокси.

Malkuth помогает автоматическим долго работающим программам выполнять четыре сложные задачи:

1. **Подключаемый транспорт** — JSON-RPC поверх локальной петли TCP, удалённого
   **WebSocket** или локального **IPC** (Unix-сокеты / именованные каналы через
   [`interprocess`](https://crates.io/crates/interprocess)). Один трейт
   `Transport`, диспетчеризуемый по схеме URL.
2. **На базе tokio, лёгкий по фреймворкам** — путь JSON-RPC не требует HTTP-фреймворка
   (axum опционален, только для HTTP-проб).
3. **Опциональные, перехватываемые возможности** — источник выхода, пробы, хуки пульса и слива
   — это *трейты*. Используйте умолчания или предоставьте свои. Достаточно оснащённый
   оркестратор `Supervised` связывает их воедино.
4. **CLI-наблюдатель** — `malkuth -- <cmd>` оборачивает программу наблюдением за файлами,
   пулом подов и закрепляющимся обратным прокси уровня 4.

## Структура рабочего пространства
Полную матрицу возможностей и использование CLI смотрите в [корневом README](../../README.md),
а архитектуру — в разделе [Проектирование](./design/supervision-and-rolling-update.md).
