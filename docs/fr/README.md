<!-- markdownlint-disable MD033 MD041 MD036 -->
<div align="center">

<img src="../res/logo.webp" alt="Plana logo" width="200"/>

# Plana

**Infrastructure permettant aux programmes longue durée de se mettre à niveau et d'équilibrer leur charge**

[![License](https://img.shields.io/badge/license-BSL--1.1-blue.svg)](../LICENSE)
[![Rust](https://img.shields.io/badge/rust-1.85%2B-orange.svg)](https://www.rust-lang.org/)
[![GitHub](https://img.shields.io/badge/github-celestia--island%2Fplana-blue.svg)](https://github.com/celestia-island/plana)

</div>
<!-- markdownlint-enable MD033 MD041 MD036 -->

**[English](../../README.md)** &bull; **[简体中文](../zhs/README.md)** &bull;
**[繁體中文](../zht/README.md)** &bull; **[日本語](../ja/README.md)** &bull;
**[한국어](../ko/README.md)** &bull; **[Français](../fr/README.md)** &bull;
**[Español](../es/README.md)** &bull; **[Русский](../ru/README.md)**

> **Version 0.1.0** — Développement précoce. Indépendant et autonome ;
> ne dépend que de tokio + axum.

Plana aide les programmes automatisés longue durée — démons, agents, serveurs — à
accomplir deux choses difficiles en toute sécurité :

- **Auto-mise à niveau** — déployer une nouvelle version (ou un build fraîchement
  compilé) sans abandonner le travail en cours ni les connexions : mises à jour
  progressives sans interruption de service.
- **Équilibrage de charge** — exécuter plusieurs instances qui se répartissent le
  travail et coordonnent l'état, où l'une peut se retirer proprement tandis qu'une
  autre prend le relais.

## Briques de base

- **Cycle de vie** — sémantique de signaux uniforme (`SIGTERM` / `SIGINT` = vidange,
  `SIGHUP` = rechargement, `SIGQUIT` = immédiat) via `DrainController`.
- **Sondes** — séparation de `/healthz` (vivacité) + `/readyz` (disponibilité, avec
  un bit de vidange) afin que les équilibreurs de charge et les orchestrateurs
  puissent router et retirer les nœuds.
- **Workers** — ressources de processus enfants supervisés, chacune constituant une
  frontière d'isolation des pannes, avec une politique de redémarrage de style OTP
  et une limitation de débit à fenêtre glissante.
- **Transmission de l'écouteur** — héritage de l'écouteur par socket-activation avec
  un repli sur un bind simple, pour des redémarrages sans interruption de service.
- **Verrous de coordination** — un trait `CoordinationLock` enfichable
  (`file-lock` / `pg-lock` / `lease`) pour coordonner les écritures concurrentes ou
  l'élection du leader.

## Démarrage rapide

```toml
[dependencies]
plana = { git = "https://github.com/celestia-island/plana.git", branch = "dev" }
# features: socket-activation, file-lock, lease, pg-lock, replica, leader-follower
```

```rust
use plana::{acquire_listener, probe_router, ProbeState, DrainController};

#[tokio::main]
async fn main() -> std::io::Result<()> {
    // Listener handoff: socket activation, falls back to a plain bind.
    let listener = acquire_listener("0.0.0.0:8080").await?;

    // Probes + signal-aware drain.
    let probe = ProbeState::new(env!("CARGO_PKG_VERSION"));
    let ctrl = DrainController::install();

    let app = axum::Router::new()
        .merge(probe_router(probe)) // GET /healthz, GET /readyz
        .with_state(());

    axum::serve(listener, app)
        .with_graceful_shutdown(async {
            // Resolves on SIGINT / SIGTERM (drain) or SIGQUIT (immediate),
            // but NOT on SIGHUP (reload — the server keeps serving).
            ctrl.wait_for_drain().await;
        })
        .await?;
    Ok(())
}
```

## Options de fonctionnalités

| Fonctionnalité | Active |
| --- | --- |
| `socket-activation` | hérite d'un fd d'écouteur (socket activation) |
| `file-lock` | backend `CoordinationLock` POSIX `flock` |
| `lease` | verrou de fichier à bail avec expiration automatique par TTL |
| `pg-lock` | backend PostgreSQL `pg_advisory_lock` (planifié) |
| `replica` | trait `InstanceRegistry` (équilibrage de charge / mise à jour progressive) |
| `leader-follower` | trait `LeaderElector` (HA actif-passif) |

## Statut

Le cycle de vie + les sondes, les workers supervisés, la transmission de l'écouteur
et le trait de verrou de coordination avec le backend `file-lock` sont implémentés.
Les backends de stratégie `replica` / `leader-follower` sont des contrats de trait
dont les implémentations complètes sont planifiées.

## Licence

Business Source License 1.1 (BSL-1.1) ; se convertit automatiquement, au choix, en
Apache-2.0 ou MIT le 2030-01-01. Voir [LICENSE](../LICENSE).
