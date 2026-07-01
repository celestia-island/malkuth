# Malkuth
<!-- markdownlint-disable MD033 MD041 MD036 -->
<div align="center">

<img src="../logo.webp" alt="Malkuth" width="200"/>

**Composable service-supervision toolkit for Rust — JSON-RPC over pluggable transports, supervised workers, coordination locks & leader election, plus a watchdog CLI.**

[![License](https://img.shields.io/badge/license-SySL%201.0-blue)](../../LICENSE)
[![Rust](https://img.shields.io/badge/rust-1.85%2B-orange.svg)](https://www.rust-lang.org/)
[![GitHub](https://img.shields.io/badge/github-celestia--island%2Fmalkuth-blue.svg)](https://github.com/celestia-island/malkuth)

</div>
<!-- markdownlint-enable MD033 MD041 MD036 -->

<!-- language switcher is available in the bottom-right corner -->

> **Version 0.1.0** — Single crate, **tokio-based**. The CLI wraps
> *any* program (even one that does not use the library) with a pod pool and a
> sticky reverse proxy.

Malkuth helps automated, long-running programs do four hard things:

1. **Pluggable transport** — JSON-RPC over local TCP loopback, remote
   **WebSocket**, or local **IPC** (Unix sockets / named pipes via
   [`interprocess`](https://crates.io/crates/interprocess)). One `Transport`
   trait, dispatched by URL scheme.
2. **Tokio-based, framework-light** — the JSON-RPC path needs no HTTP framework
   (axum is optional, for HTTP probes only).
3. **Optional, hookable facilities** — exit source, probes, heartbeat and drain
   hooks are *traits*. Use the defaults or supply your own. A batteries-included
   `Supervised` orchestrator wires them together.
4. **A watchdog CLI** — `malkuth -- <cmd>` wraps a program with file watching, a
   pod pool, and an L4 sticky reverse proxy.

## Workspace layout
See the [root README](../../README.md) for the full feature matrix and the CLI
usage, and [Design](./design/supervision-and-rolling-update.md) for the
architecture.
