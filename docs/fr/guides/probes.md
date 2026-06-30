# Sondes de santé

## Sondes séparées : `/healthz` vs `/readyz`

Malkuth suit la convention Kubernetes des **deux endpoints de sondes distincts** :

- **`GET /healthz`** —— *Vitalité* : « Le processus est-il en vie ? » En cas
  d'échec, l'orchestrateur **redémarre** l'instance.
- **`GET /readyz`** —— *Préparation* : « Cette instance peut-elle servir du
  trafic maintenant ? » En cas d'échec, l'orchestrateur **arrête de router le
  trafic** mais ne redémarre pas.

La distinction compte lors des mises à jour progressives : une instance en cours
de vidange est *en vie* (healthz = 200) mais *pas prête* (readyz = 503).

## Configuration

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

## Forme des réponses

### `/healthz` (toujours 200 si le processus peut répondre)

```json
{
  "alive": true,
  "pid": 12345,
  "uptime_secs": 3600,
  "version": "0.1.0"
}
```

### `/readyz` (503 quand non prêt)

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

Pendant la vidange ou si une dépendance est défaillante, `ready` vaut `false` et le
statut HTTP est `503 Service Unavailable`.

## Câbler le bit de vidange

Pendant l'arrêt progressif, positionnez le drapeau de vidange pour que `/readyz`
commence à renvoyer 503 :

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

Désormais, l'équilibreur de charge voit `/readyz` passer à 503 et cesse d'envoyer du
nouveau trafic **avant** que le processus ne se termine — le cœur des mises à jour
progressives sans interruption.

## Génération de déploiement

Suivez à quelle génération de déploiement appartient cette instance :

```rust
probe.set_generation(Some(2)).await; // generation 2 of a rolling update
```

Ceci est inclus dans la réponse `/readyz` pour l'observabilité et l'orchestration.
