# Apagado elegante y drenaje

## El problema

La mayoría de los servidores Rust solo capturan `ctrl_c` (SIGINT). Pero
`docker stop`, `systemctl restart` y la terminación de pods de Kubernetes envían
**SIGTERM** — lo que ignora tu apagado elegante y mata las solicitudes en curso.

## La solución: `DrainController`

`DrainController::install()` configura manejadores de señales canónicos siguiendo
la convención de nginx/Go:

| Señal | Significado | ¿Drenar? |
| --- | --- | --- |
| `SIGINT` / `SIGTERM` | Apagado elegante | Sí |
| `SIGHUP` | Recarga en caliente de configuración | No (el servidor sigue sirviendo) |
| `SIGQUIT` | Salida inmediata | Sí (omite el drenaje) |

## Uso

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

## Integración en `axum::serve`

```rust
axum::serve(listener, app)
    .with_graceful_shutdown(async {
        ctrl.wait_for_drain().await;
    })
    .await?;
```

`wait_for_drain` se completa con `SIGINT`/`SIGTERM`/`SIGQUIT` pero **no** con
`SIGHUP`, de modo que una recarga no detiene el servidor accidentalmente.

## Observar el estado de drenaje

```rust
// Non-blocking check:
if ctrl.is_draining() {
    // refuse new work
}

// Sleep, but wake early if drain begins:
ctrl.sleep_or_drain(std::time::Duration::from_secs(30)).await;
```

## Drenaje programático

También puedes activar el drenaje desde dentro del proceso (por ejemplo, un RPC
de administración):

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
