# Sondas de salud

## Sondas separadas: `/healthz` vs `/readyz`

Malkuth sigue la convención de Kubernetes de **dos endpoints de sondas separados**:

- **`GET /healthz`** —— *Disponibilidad*: "¿El proceso está vivo?" Si falla, el
  orquestador **reinicia** la instancia.
- **`GET /readyz`** —— *Preparación*: "¿Puede esta instancia servir tráfico ahora
  mismo?" Si falla, el orquestador **deja de enrutar tráfico** pero no reinicia.

La distinción importa durante las actualizaciones continuas: una instancia que está
drenando está *viva* (healthz = 200) pero *no lista* (readyz = 503).

## Configuración

```rust
use malkuth::{ProbeState, probe_router};

let probe = ProbeState::new(env!("CARGO_PKG_VERSION"));

// Register dependencies that affect readiness:
probe.add_dependency("database", || {
    // Return true if the DB connection is healthy.
    // This is a sync closure — keep it cheap (read an atomic, ping a cached conn).
    true
}).await;

// Merge the probe routes into your app:
let app = axum::Router::new()
    .merge(probe_router(probe));
```

## Formato de las respuestas

### `/healthz` (siempre 200 si el proceso puede responder)

```json
{
  "alive": true,
  "pid": 12345,
  "uptime_secs": 3600,
  "version": "0.1.0"
}
```

### `/readyz` (503 cuando no está listo)

```json
{
  "ready": true,
  "draining": false,
  "dependencies": [
    { "name": "database", "ok": true }
  ],
  "generation": 2
}
```

Cuando se está drenando o una dependencia está caída, `ready` es `false` y el estado
HTTP es `503 Service Unavailable`.

## Conectar el bit de drenaje

Durante el apagado elegante, activa el indicador de drenaje para que `/readyz`
empiece a devolver 503:

```rust
let probe = ProbeState::new(env!("CARGO_PKG_VERSION"));
let ctrl = DrainController::install();

// In your shutdown sequence:
tokio::spawn({
    let probe = probe.clone();
    let ctrl = ctrl.clone();
    async move {
        // Wait until drain begins, then flip the bit.
        ctrl.wait_for_drain().await;
        probe.set_draining(true).await;
    }
});
```

Así el balanceador de carga ve que `/readyz` pasa a 503 y deja de enviar nuevo
tráfico **antes** de que el proceso termine — el núcleo de las actualizaciones
continuas sin tiempo de inactividad.

## Generación de despliegue

Rastrea a qué generación de despliegue pertenece esta instancia:

```rust
probe.set_generation(Some(2)).await; // generation 2 of a rolling update
```

Esto se incluye en la respuesta de `/readyz` para observabilidad y orquestación.
