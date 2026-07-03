# Supervision de workers

## Le modèle

Un **worker** est un processus enfant indépendamment tuable qui détient exactement
une ressource (une connexion PLC, un port série, un sidecar comme cosmos ou
pglite-proxy). Le processus enfant est la **frontière d'isolation de pannes** :
si la ressource plante, seul le worker redémarre — le parent continue de servir.

## Définir des workers

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

## Politiques de redémarrage

Empruntées à Erlang/OTP :

| Politique | Redémarre quand… |
| --- | --- |
| `Permanent` (par défaut) | Toute sortie, même propre |
| `Transient` | Sortie anormale (non nulle) uniquement |
| `Temporary` | Jamais |

## Limitation de débit

Le superviseur applique une **limitation de débit à fenêtre glissante** pour
prévenir les tempêtes de crash :

```rust
let supervisor = Supervisor::new(workers)
    .rate_limit(5, std::time::Duration::from_secs(60)) // max 5 restarts / 60s
    .cooldown(std::time::Duration::from_secs(30));      // then cooldown 30s
```

Si un worker plante plus de `max_restarts` fois dans la fenêtre, il entre dans
une période de refroidissement avant la prochaine tentative.

## Exécuter le superviseur

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

`Supervisor::run` fait courir la sortie de chaque enfant contre
`wait_for_drain()`. Lors de la vidange, tous les enfants sont tués
(`kill_on_drop`) et les instantanés `WorkerInfo` finaux sont renvoyés.

## Instantanés d'état des workers

Après que `supervisor.run()` s'est terminé, il renvoie un `Vec<WorkerInfo>` avec
l'état final de chaque worker, son nombre de redémarrages et sa dernière
erreur :

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
