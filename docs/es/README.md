# Malkuth
<!-- markdownlint-disable MD033 MD041 MD036 -->
<div align="center">

<img src="../logo.webp" alt="Malkuth" width="200"/>

**Kit de herramientas componible para la supervisión de servicios en Rust — JSON-RPC sobre transportes conectables, workers supervisados, cerraduras de coordinación y elección de líder, más un CLI watchdog.**

[![License](https://img.shields.io/badge/license-SySL%201.0-blue)](../../LICENSE)
[![Rust](https://img.shields.io/badge/rust-1.85%2B-orange.svg)](https://www.rust-lang.org/)
[![GitHub](https://img.shields.io/badge/github-celestia--island%2Fmalkuth-blue.svg)](https://github.com/celestia-island/malkuth)

</div>
<!-- markdownlint-enable MD033 MD041 MD036 -->

<!-- language switcher is available in the bottom-right corner -->

> **Versión 0.1.0** — Un único crate, **basado en tokio**. El CLI envuelve
> *cualquier* programa (incluso uno que no use la biblioteca) con un pool de pods y un
> proxy inverso persistente.

Malkuth ayuda a los programas automatizados y de larga duración a hacer cuatro cosas difíciles:

1. **Transporte conectable** — JSON-RPC sobre un bucle de retorno TCP local, una
   **WebSocket** remota o un **IPC** local (sockets de Unix / tuberías con nombre vía
   [`interprocess`](https://crates.io/crates/interprocess)). Un único trait
   `Transport`, despachado según el esquema de URL.
2. **Basado en tokio, ligero de framework** — la ruta JSON-RPC no necesita ningún framework HTTP
   (axum es opcional, solo para sondas HTTP).
3. **Facilidades opcionales y conectables mediante hooks** — la fuente de salida, las sondas, los hooks de latido y de drenaje
   son *traits*. Usa los predeterminados o aporta los tuyos. Un orquestador
   `Supervised` «con pilas incluidas» los cablea entre sí.
4. **Un CLI watchdog** — `malkuth -- <cmd>` envuelve un programa con observación de archivos, un
   pool de pods y un proxy inverso persistente de capa 4.

## Estructura del espacio de trabajo
Consulta el [README raíz](../../README.md) para la matriz completa de funcionalidades y el uso
del CLI, y [Diseño](./design/supervision-and-rolling-update.md) para
la arquitectura.
