# Supervisión de workers

## El modelo

Un **worker** es un proceso hijo terminable de forma independiente, que posee exactamente
un recurso (una conexión PLC, un puerto serie, un sidecar como cosmos o pglite-proxy).
El proceso hijo es el **límite de aislamiento de fallos**: si el recurso se cae,
solo el worker se reinicia — el padre sigue sirviendo.

## Definir workers

```rust
use malkuth::{Supervisor, WorkerSpec};
use malkuth::RestartPolicy;

let workers = vec![
    WorkerSpec::new("plc-1", "modbus", "/usr/bin/modbus-bridge")
        .args(["--device", "/dev/ttyUSB0"])
        .policy(RestartPolicy::Permanent),

    WorkerSpec::new("cosmos", "cosmos", "/usr/bin/cosmos-agent")
        .policy(RestartPolicy::Transient), // restart only on abnormal exit
];
```

## Políticas de reinicio

Tomadas de Erlang/OTP:

| Política | Reinicia ante… |
| --- | --- |
| `Permanent` (predeterminado) | Cualquier salida, incluso una limpia |
| `Transient` | Solo salida anormal (distinta de cero) |
| `Temporary` | Nunca |

## Limitación de tasa

El supervisor aplica una **limitación de tasa de ventana deslizante** para prevenir tormentas de caídas:

```rust
let supervisor = Supervisor::new(workers)
    .rate_limit(5, std::time::Duration::from_secs(60)) // max 5 restarts / 60s
    .cooldown(std::time::Duration::from_secs(30));      // then cooldown 30s
```

Si un worker se cae más de `max_restarts` veces dentro de la ventana, entra en un
período de enfriamiento antes del próximo intento.

## Ejecutar el supervisor

```rust
use tokio::sync::watch;

let (shutdown_tx, shutdown_rx) = watch::channel(false);

let supervisor = Supervisor::new(workers)
    .rate_limit(5, std::time::Duration::from_secs(60));

// Run until shutdown signal:
tokio::spawn(async move {
    let final_status = supervisor.run(shutdown_rx).await;
    for w in &final_status {
        tracing::info!(worker = %w.id, status = ?w.status, restarts = w.restart_count, "final");
    }
});

// Later, trigger shutdown:
let _ = shutdown_tx.send(true);
```

## Instantáneas del estado de los workers

Tras completarse `supervisor.run()` (al apagarse), devuelve un `Vec<WorkerInfo>`
con el estado final, el conteo de reinicios y el último error de cada worker —
útil para registro o para reportar a un sistema de monitoreo.
