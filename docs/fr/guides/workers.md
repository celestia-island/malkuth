# Supervision des workers

## Le modèle

Un **worker** est un processus fils arrêtable indépendamment, qui détient exactement
une ressource (une connexion PLC, un port série, un sidecar comme cosmos ou pglite-proxy).
Le processus fils constitue la **frontière d'isolation des pannes** : si la ressource plante,
seul le worker redémarre — le parent continue de servir.

## Définir des workers

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

## Politiques de redémarrage

Empruntées à Erlang/OTP :

| Politique | Redémarre si… |
| --- | --- |
| `Permanent` (par défaut) | Toute sortie, même propre |
| `Transient` | Sortie anormale (non nulle) uniquement |
| `Temporary` | Jamais |

## Limitation de débit

Le superviseur applique une **limitation de débit à fenêtre glissante** pour éviter les tempêtes de crash :

```rust
let supervisor = Supervisor::new(workers)
    .rate_limit(5, std::time::Duration::from_secs(60)) // max 5 restarts / 60s
    .cooldown(std::time::Duration::from_secs(30));      // then cooldown 30s
```

Si un worker plante plus de `max_restarts` fois dans la fenêtre, il entre dans une
période de refroidissement avant la prochaine tentative.

## Exécuter le superviseur

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

## Instantanés d'état des workers

Une fois `supervisor.run()` terminé (à l'arrêt), il renvoie un `Vec<WorkerInfo>`
contenant l'état final, le nombre de redémarrages et la dernière erreur de chaque worker —
utile pour la journalisation ou le rapport à un système de supervision.
