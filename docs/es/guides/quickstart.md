# Inicio rápido

## Añadir la dependencia

```toml
[dependencies]
malkuth = { git = "https://github.com/celestia-island/malkuth.git", branch = "dev" }
# features: tcp (default) | ws | ipc | signals (default) | worker | probes |
#           file-lock | lease | pg-lock | replica | leader-follower | schema
```

## Un servicio JSON-RPC mínimo

```rust
use std::sync::Arc;
use malkuth::{Router, Supervised};
use malkuth::transport::TcpTransport;
use malkuth::Transport;
use serde_json::json;

#[tokio::main]
async fn main() -> std::io::Result<()> {
    let lis = TcpTransport.listen("tcp://127.0.0.1:0").await?;
    let supervised = Supervised::new().signals();
    let ctrl = supervised.drain_controller();

    let handler = Arc::new(
        Router::new()
            .lifecycle(ctrl, None)            // registers Lifecycle.Drain / Status / Health / Reload
            .route("ping", |_| Box::pin(async { Ok(json!("pong")) })),
    );

    supervised.serve_rpc_listener(lis, handler).await
}
```

`Supervised` hace competir al servidor JSON-RPC contra la fuente de salida por
señal del SO (SIGINT/SIGTERM → drenaje, SIGHUP → recarga, SIGQUIT → salida
inmediata), y luego ejecuta los hooks de drenaje registrados. Sustituye
`.signals()` por `.exit(your_impl)` para disparar el drenaje desde tu propia
lógica.

## Llamarlo desde un cliente

```rust
use malkuth::Client;
use malkuth::transport::TcpTransport;
use malkuth::Transport;
use serde_json::json;

let mut c = Client::connect(&TcpTransport, "tcp://127.0.0.1:8080").await?;

// Custom method:
let r = c.call("ping", json!({})).await?;       // → "pong"

// Standard lifecycle methods (registered by Router::lifecycle):
c.notify("Lifecycle.Drain", json!({})).await?;  // → server begins graceful drain
let health = c.call("Lifecycle.Health", json!({})).await?;
// → { "alive": true, "pid": 12345, "uptime_secs": 360, "version": "0.1.0" }
let status = c.call("Lifecycle.Status", json!({})).await?;
// → { "ready": true, "draining": false, "dependencies": [], "generation": null }
```

## El protocolo de ciclo de vida JSON-RPC

`Router::lifecycle(drain, probe)` registra cuatro métodos estándar:

| Método | Parámetros | Resultado | Efecto |
| --- | --- | --- | --- |
| `Lifecycle.Drain` | `{}` | `{ "accepted": true, "draining": true }` | Inicia el drenaje gracioso |
| `Lifecycle.Reload` | `{}` | `null` | Inicia la recarga (sin salir) |
| `Lifecycle.Status` | `{}` | `ReadyStatus` | Consulta la preparación (bit de drenaje + dependencias) |
| `Lifecycle.Health` | `{}` | `HealthStatus` | Consulta la vivacidad (pid / uptime / versión) |

Todos los mensajes son JSON-RPC 2.0 enmarcado en NDJSON sobre el transporte
elegido.

## Otros transportes

Cambia `TcpTransport` por `WsTransport` (feature `ws`, dirección `ws://host:port`)
o `IpcTransport` (feature `ipc`, dirección `ipc:/tmp/sock`). O usa
`MultiTransport`, que despacha según el esquema de la URL
(`tcp://` / `ws://` / `ipc:`).

## Envolver cualquier programa con el CLI

```bash
malkuth --watch ./src --proxy 3000:3000-3999 --pod-count 3 -- cargo run
```

Esto lanza 3 pods (autoasignando los puertos 3001–3003 mediante la variable de
entorno `PORT`), sondea cada uno hasta que está escuchando, y los frontaliza con
un reverse proxy persistente en el puerto 3000 (enrutado por hash consistente
según la IP del cliente). Un cambio bajo `./src` desencadena un reinicio
progresivo, un pod a la vez.
