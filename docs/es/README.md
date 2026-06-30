# Malkuth
<!-- markdownlint-disable MD033 MD041 MD036 -->
<div align="center">

<img src="../logo.webp" alt="Malkuth" width="200"/>


**Infraestructura para que programas de larga duración se auto-actualicen y balanceen la carga**

[![License](https://img.shields.io/badge/license-BSL--1.1-blue.svg)](LICENSE)
[![Rust](https://img.shields.io/badge/rust-1.85%2B-orange.svg)](https://www.rust-lang.org/)
[![GitHub](https://img.shields.io/badge/github-celestia--island%2Fmalkuth-blue.svg)](https://github.com/celestia-island/malkuth)

</div>
<!-- markdownlint-enable MD033 MD041 MD036 -->

**[English](../en/README.md)** &bull; **[简体中文](../zhs/README.md)** &bull;
**[繁體中文](../zht/README.md)** &bull; **[日本語](../ja/README.md)** &bull;
**[한국어](../ko/README.md)** &bull; **[Français](../fr/README.md)** &bull;
**[Español](README.md)** &bull; **[Русский](../ru/README.md)**

> **Versión 0.1.0** — Desarrollo temprano. Independiente y autónomo;
> depende únicamente de tokio + axum.

Malkuth ayuda a los programas automatizados y de larga duración — demonios,
agentes, servidores — a hacer dos cosas difíciles de forma segura:

- **Auto-actualización** — desplegar una nueva versión (o una compilación
  recién generada) sin perder trabajo en curso ni conexiones: actualizaciones
  continuas sin tiempo de inactividad.
- **Balanceo de carga** — ejecutar múltiples instancias que comparten el
  trabajo y coordinan el estado, donde una puede retirarse de forma elegante
  mientras otra toma el relevo.

## Bloques de construcción

- **Ciclo de vida** — semántica uniforme de señales (`SIGTERM` / `SIGINT` =
  drenaje, `SIGHUP` = recarga, `SIGQUIT` = inmediato) mediante
  `DrainController`.
- **Sondas** — `/healthz` (disponibilidad) + `/readyz` (preparación, con un
  bit de drenaje) separados, para que los balanceadores de carga y los
  orquestadores puedan enrutar y retirar nodos.
- **Workers** — recursos de procesos hijos supervisados, cada uno un límite de
  aislamiento de fallos, con política de reinicio estilo OTP y limitación de
  tasa por ventana deslizante.
- **Traspaso de listener** — herencia del listener mediante socket activation
  con un respaldo de bind simple, para reinicios sin tiempo de inactividad.
- **Cerraduras de coordinación** — un trait `CoordinationLock` conectable
  (`file-lock` / `pg-lock` / `lease`) para coordinar escrituras concurrentes o
  elección de líder.

## Inicio rápido

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

## Banderas de características

| Característica | Habilita |
| --- | --- |
| `socket-activation` | heredar un fd de listener (socket activation) |
| `file-lock` | backend `CoordinationLock` con `flock` POSIX |
| `lease` | bloqueo de archivo basado en lease con expiración automática por TTL |
| `pg-lock` | backend `pg_advisory_lock` de PostgreSQL (en preparación) |
| `replica` | trait `InstanceRegistry` (balanceo de carga / actualización continua) |
| `leader-follower` | trait `LeaderElector` (HA activo-pasivo) |

## Estado

El ciclo de vida + las sondas, los workers supervisados, el traspaso de
listener y el trait de cerradura de coordinación con el backend `file-lock`
están implementados. Los backends de estrategia `replica` / `leader-follower`
son contratos de trait con implementaciones completas en preparación. Consulte
[docs/design/](../en/design/) para el diseño.

## Licencia

Business Source License 1.1 (BSL-1.1); se convierte automáticamente a tu
elección de Apache-2.0 o MIT el 2030-01-01. Consulta [LICENSE](../../LICENSE).
