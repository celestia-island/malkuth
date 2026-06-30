# Malkuth
<!-- markdownlint-disable MD033 MD041 MD036 -->
<div align="center">

<img src="../logo.webp" alt="Malkuth" width="200"/>


**Infrastructure permettant aux programmes longue durée de s'auto-mettre à jour et d'équilibrer la charge**

[![License](https://img.shields.io/badge/license-BSL--1.1-blue.svg)](../../LICENSE)
[![Rust](https://img.shields.io/badge/rust-1.85%2B-orange.svg)](https://www.rust-lang.org/)
[![GitHub](https://img.shields.io/badge/github-celestia--island%2Fmalkuth-blue.svg)](https://github.com/celestia-island/malkuth)

</div>
<!-- markdownlint-enable MD033 MD041 MD036 -->

**[English](../en/README.md)** &bull; **[简体中文](../zhs/README.md)** &bull;
**[繁體中文](../zht/README.md)** &bull; **[日本語](../ja/README.md)** &bull;
**[한국어](../ko/README.md)** &bull; **[Français](README.md)** &bull;
**[Español](../es/README.md)** &bull; **[Русский](../ru/README.md)**

> **Version 0.1.0** — Développement précoce. Indépendant et autonome ;
> ne dépend que de tokio + axum.

Malkuth aide les programmes automatisés longue durée — démons, agents,
serveurs — à réaliser deux choses difficiles en toute sécurité :

- **Auto-mise à jour** — déployer une nouvelle version (ou un build fraîchement
  compilé) sans perdre le travail en cours ni les connexions : mises à jour
  progressives sans interruption.
- **Équilibrage de charge** — exécuter plusieurs instances qui se répartissent
  le travail et coordonnent leur état, où l'une peut s'arrêter proprement tandis
  qu'une autre prend le relais.

## Blocs constitutifs

- **Cycle de vie** — sémantique uniforme des signaux (`SIGTERM` / `SIGINT` =
  vidange, `SIGHUP` = rechargement, `SIGQUIT` = immédiat) via `DrainController`.
- **Sondes** — séparation de `/healthz` (vitalité) + `/readyz` (préparation,
  avec un bit de vidange) afin que les équilibreurs de charge et les
  orchestrateurs puissent router et retirer les nœuds.
- **Workers** — ressources de processus enfants supervisés, chacune constituant
  une frontière d'isolation des pannes, avec une politique de redémarrage de
  type OTP et une limitation de débit à fenêtre glissante.
- **Passation de listener** — héritage de listener par activation de socket avec
  un repli sur bind simple, pour des redémarrages sans interruption.
- **Verrous de coordination** — un trait `CoordinationLock` enfichable
  (`file-lock` / `pg-lock` / `lease`) pour coordonner les écritures concurrentes
  ou l'élection du leader.

## Démarrage rapide

```toml
[dependencies]
malkuth = { git = "https://github.com/celestia-island/malkuth.git", branch = "dev" }
# features: socket-activation, file-lock, lease, pg-lock, replica, leader-follower
```

```rust
use malkuth::{acquire_listener, probe_router, ProbeState, DrainController};

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

## Fonctionnalités optionnelles

| Fonctionnalité | Active |
| --- | --- |
| `socket-activation` | héritage d'un fd de listener (activation de socket) |
| `file-lock` | backend `CoordinationLock` POSIX `flock` |
| `lease` | verrou fichier à bail avec expiration automatique par TTL |
| `pg-lock` | backend PostgreSQL `pg_advisory_lock` (planifié) |
| `replica` | trait `InstanceRegistry` (équilibrage de charge / mise à jour progressive) |
| `leader-follower` | trait `LeaderElector` (HA actif-passif) |

## Statut

Le cycle de vie + les sondes, les workers supervisés, la passation de listener
et le trait de verrou de coordination avec le backend `file-lock` sont
implémentés. Les backends de stratégie `replica` / `leader-follower` sont des
contrats de trait avec des implémentations complètes planifiées. Consultez
[docs/design/](design/) pour la conception.

## Licence

Business Source License 1.1 (BSL-1.1) ; se convertit automatiquement, au choix,
en Apache-2.0 ou MIT le 2030-01-01. Voir [LICENSE](../../LICENSE).
