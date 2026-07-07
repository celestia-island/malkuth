<p align="center"><img src="https://raw.githubusercontent.com/celestia-island/malkuth/master/docs/logo.webp" alt="Malkuth" width="240" /></p>

<h1 align="center">Malkuth</h1>

<p align="center"><strong>Kit de herramientas componible para la supervisión de servicios en Rust</strong></p>

<div align="center">

[![License: SySL-1.0](https://img.shields.io/badge/License-SySL--1.0-blue.svg)](https://sysl.celestia.world)
[![GitHub](https://img.shields.io/badge/github-celestia--island%2Fmalkuth-blue.svg)](https://github.com/celestia-island/malkuth)
[![Checks](https://img.shields.io/github/actions/workflow/status/celestia-island/malkuth/checks.yml)](https://github.com/celestia-island/malkuth/actions/workflows/checks.yml)
[![Docs](https://img.shields.io/badge/docs-malkuth.docs.celestia.world-blue)](https://malkuth.docs.celestia.world)
[![docs.rs](https://docs.rs/malkuth/badge.svg)](https://docs.rs/malkuth)

</div>

<div align="center">

[English](../en/README.md) · [简体中文](../zhs/README.md) · [繁體中文](../zht/README.md) · [日本語](../ja/README.md) · [한국어](../ko/README.md) · [Français](../fr/README.md) · **Español** · [Русский](../ru/README.md) · [العربية](../ar/README.md)

</div>

Malkuth ayuda a los programas automatizados y de larga duración a hacer cuatro cosas difíciles:

1. **Transporte conectable** — JSON-RPC sobre bucle de retorno TCP local, **WebSocket** remoto o **IPC** local (sockets Unix / tuberías con nombre vía
   [`interprocess`](https://crates.io/crates/interprocess)). Un único trait
   `Transport`, despachado según el esquema de URL.
2. **Trabajadores supervisados** — lanzar un proceso, monitorizar su salud, reiniciarlo en caso de fallo, drenar conexiones antes de apagar.
3. **Facilidades opcionales y conectables mediante hooks** — la fuente de salida, las sondas, los hooks de latido y de drenaje son *traits*. Usa los predeterminados (señal de salida del SO, sondas axum, workers supervisados) o proporciona los tuyos (p. ej. activar el drenaje desde un comando "stop" en banda que reciba tu servidor). Un orquestador `Supervised` «con pilas incluidas» los conecta entre sí.
4. **Un CLI watchdog** — `malkuth -- <cmd>` envuelve un programa con observación de archivos, un pool de pods y un proxy inverso persistente de capa 4.

## Como CLI

```
malkuth [--watch PATH]... [--proxy PUBLIC:LO-HI] [--pod-count N] -- <cmd> [args...]
```

Ejecuta 5 copias paralelas de tu servidor (cada una escuchando en la variable de entorno `PORT` → se autoasignan 3001–3005), frente a un proxy persistente en el puerto 3000:

```bash
malkuth --watch ./src --watch ./res \
        --proxy 3000:3000-3999 --pod-count 5 \
        -- cargo run
```

El proxy enruta cada **IP de cliente** a un backend fijo mediante hashing consistente, de modo que un cliente siga alcanzando el mismo pod hasta que ese pod se reinicie o se reduzca la escala — la base para lanzamientos graduales / reinicios progresivos. Ante un cambio de archivo, drena y reinicia un pod a la vez.

## Como biblioteca

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

¿Necesitas que el drenaje se active por tu propia lógica en lugar de por señales? Implementa
`malkuth::ExitSource` y pásalo mediante `.exit(...)`. ¿Quieres coordinación respaldada por Postgres?
La funcionalidad `pg-lock` proporciona un backend `CoordinationLock`.

## Banderas de funcionalidad (feature flags)

| Funcionalidad | Habilita |
| --- | --- |
| `tcp` *(default)* | JSON-RPC sobre TCP local/remoto (`tokio::net`) |
| `ws` | JSON-RPC sobre WebSocket (`tokio-tungstenite`) |
| `ipc` | JSON-RPC sobre IPC local (`interprocess`) |
| `signals` *(default)* | `ExitSource` por defecto basado en señales del SO (`tokio::signal`) |
| `worker` | Workers supervisados como procesos hijos (`tokio::process`) |
| `probes` | Router axum `/healthz` + `/readyz` |
| `file-lock` | Backend `CoordinationLock` con `flock` POSIX (unix) |
| `lease` | `CoordinationLock` con arrendamiento de archivo y expiración automática por TTL (seguro ante fallos) |
| `pg-lock` | Backend `pg_advisory_lock` de PostgreSQL (`tokio-postgres`) |
| `replica` | `InstanceRegistry` en memoria |
| `leader-follower` | `LeaseLeaderElector` (sobre el backend de arrendamiento) |
| `schema` | Derivaciones `schemars::JsonSchema` para los tipos de cable |
| `cli` | El binario watchdog `malkuth` (pool de pods + proxy persistente) |

## Estado

Las capas 1–3 (ciclo de vida/drenaje, sondas, transferencia de escucha) y el núcleo JSON-RPC
(codec + servidor/cliente + transportes tcp/ws/ipc) están implementados y probados
de extremo a extremo. El pool de pods del CLI + proxy persistente está funcionando (verificado e2e). Los tres backends `CoordinationLock` (`file-lock`, `lease`, `pg-lock`) y el `LeaseLeaderElector` de `leader-follower` están implementados. Consulta
[docs/design/](../en/design/) para el diseño.

## Licencia

SySL-1.0 (Synthetic Source License). Consulte [LICENSE](https://sysl.celestia.world).
