# Entrega del listener

## El problema

Cuando un proceso servidor se reinicia, existe una ventana en la que nadie está escuchando
en el puerto — las conexiones entrantes se pierden. Para actualizaciones continuas sin
tiempo de inactividad, el nuevo proceso debe heredar el socket de escucha del anterior.

## La solución: socket activation

systemd (o un lanzador personalizado) mantiene abierto el fd del socket de escucha. Cuando el
proceso se reinicia, el nuevo proceso hereda el fd y puede aceptar conexiones de inmediato —
el núcleo las encola durante el intervalo.

El `acquire_listener` de Malkuth implementa esto en **Rust puro** (sin `libsystemd`):

```rust
use malkuth::acquire_listener;

// Prefers systemd socket activation (fd inherited), falls back to a plain bind.
let listener = acquire_listener("0.0.0.0:8080").await?;
```

Habilita la característica `socket-activation`:

```toml
[dependencies]
malkuth = { git = "https://github.com/celestia-island/malkuth.git", branch = "dev", features = ["socket-activation"] }
```

## Cómo funciona

systemd establece dos variables de entorno:

| Variable | Significado |
| --- | --- |
| `LISTEN_PID` | PID del proceso que debe heredar los fds (debe ser el nuestro) |
| `LISTEN_FDS` | Número de fds pasados (empezando en fd 3) |

Malkuth los lee, valida que `LISTEN_PID == our_pid`, toma posesión de fd 3
(`SD_LISTEN_FDS_START`), lo pone en modo no bloqueante y lo envuelve en un
`tokio::net::TcpListener`.

Si las variables están ausentes o el PID no coincide, recurre a
`TcpListener::bind(addr)`.

## Ejemplo de unidad de systemd

```ini
# /etc/systemd/system/myapp.socket
[Socket]
ListenStream=8080

[Install]
WantedBy=sockets.target
```

```ini
# /etc/systemd/system/myapp.service
[Service]
ExecStart=/usr/bin/myapp
# systemd passes the socket fd automatically when the socket unit is active
```

Con esta configuración, `systemctl restart myapp` no pierde ninguna conexión en curso:
el núcleo las retiene en la cola de escucha mientras el nuevo proceso arranca y hereda el fd.
