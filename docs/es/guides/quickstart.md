# Inicio rápido

## Añadir malkuth a tu proyecto

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

## Servidor minimal con apagado elegante y sondas

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

## Lo que obtienes

| Endpoint | Propósito |
| --- | --- |
| `GET /healthz` | Disponibilidad — «el proceso está vivo» (pid, tiempo de actividad, versión) |
| `GET /readyz` | Preparación — «puede servir tráfico» (devuelve 503 durante el drenaje) |

| Señal | Comportamiento |
| --- | --- |
| `SIGINT` / `SIGTERM` | Drenaje elegante (terminar trabajo en curso, luego salir) |
| `SIGHUP` | Recarga en caliente (**no sale** — el servidor sigue sirviendo) |
| `SIGQUIT` | Salida inmediata (solo emergencias) |

## Banderas de características

| Característica | Lo que habilita |
| --- | --- |
| `socket-activation` | Heredar un fd de listener de systemd (reinicio sin tiempo de inactividad) |
| `file-lock` | Backend `CoordinationLock` basado en `flock` POSIX |
| `lease` | Bloqueo de archivo basado en lease con expiración automática por TTL al caer |
| `replica` | Trait `InstanceRegistry` para réplicas con balanceo de carga |
| `leader-follower` | Trait `LeaderElector` para HA activo-pasivo |

Todas las características son opt-in; la compilación por defecto no tiene código unsafe y solo depende de tokio + axum.
