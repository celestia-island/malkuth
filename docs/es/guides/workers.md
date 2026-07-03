# Supervisión de workers

## El modelo

Un **worker** es un proceso hijo independiente y matable que retiene exactamente
un recurso (una conexión PLC, un puerto serie, un sidecar como cosmos o
pglite-proxy). El proceso hijo es la **frontera de aislamiento de fallos**: si el
recurso cae, solo se reinicia el worker — el padre sigue sirviendo.

## Definir workers

```rust
use malkuth::{Supervisor, WorkerSpec, RestartPolicy, DrainController};

let workers = vec![
    WorkerSpec::new("plc-1", "modbus", "/usr/bin/modbus-bridge")
        .args(["--device", "/dev/ttyUSB0"])
        .env("LOG_LEVEL", "debug")
        .policy(RestartPolicy::Permanent),

    WorkerSpec::new("cosmos", "cosmos", "/usr/bin/cosmos-agent")
        .policy(RestartPolicy::Transient), // restart only on abnormal exit
];
```

## Políticas de reinicio

Tomadas en préstamo de Erlang/OTP:

| Política | Reinicia cuando… |
| --- | --- |
| `Permanent` (predeterminada) | Cualquier salida, incluso una limpia |
| `Transient` | Solo salida anormal (no cero) |
| `Temporary` | Nunca |

## Limitación de tasa

El supervisor aplica una **limitación de tasa de ventana deslizante** para evitar
tormentas de caídas:

```rust
let supervisor = Supervisor::new(workers)
    .rate_limit(5, std::time::Duration::from_secs(60)) // max 5 restarts / 60s
    .cooldown(std::time::Duration::from_secs(30));      // then cooldown 30s
```

Si un worker cae más de `max_restarts` veces dentro de la ventana, entra en un
periodo de enfriamiento antes del siguiente intento.

## Ejecutar el supervisor

```rust
use malkuth::DrainController;

let drain = DrainController::new();
let supervisor = Supervisor::new(workers)
    .rate_limit(5, std::time::Duration::from_secs(60));

// Run until drain signal:
tokio::spawn(async move {
    let final_status = supervisor.run(drain).await;
    for w in &final_status {
        tracing::info!(worker = %w.id, status = ?w.status, restarts = w.restart_count, "final");
    }
});

// Later, trigger shutdown:
// drain.begin_drain(ShutdownKind::Graceful);
```

`Supervisor::run` hace competir la salida de cada hijo contra `wait_for_drain()`.
Al drenar, todos los hijos se matan (`kill_on_drop`) y se devuelven las
instantáneas finales de `WorkerInfo`.

## Instantáneas de estado de los workers

Tras completarse `supervisor.run()`, devuelve un `Vec<WorkerInfo>` con el estado
final de cada worker, su número de reinicios y su último error:

```rust
pub struct WorkerInfo {
    pub id: String,
    pub kind: String,
    pub status: WorkerStatus,     // Starting | Running | Stopped | Failed
    pub restart_policy: RestartPolicy,
    pub restart_count: u32,
    pub last_error: Option<String>,
}
```
