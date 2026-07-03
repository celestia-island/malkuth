<p align="center"><img src="../logo.webp" alt="Malkuth" width="240" /></p>

<h1 align="center">Malkuth</h1>

<p align="center"><strong>Boîte à outils composable de supervision de services pour Rust</strong></p>

<div align="center">

[![License](https://img.shields.io/badge/license-SySL%201.0-blue)](../../LICENSE) [![Rust](https://img.shields.io/badge/rust-1.85%2B-orange.svg)](https://www.rust-lang.org/) [![GitHub](https://img.shields.io/badge/github-celestia--island%2Fmalkuth-blue.svg)](https://github.com/celestia-island/malkuth) [![Checks](https://img.shields.io/github/actions/workflow/status/celestia-island/malkuth/checks.yml)](https://github.com/celestia-island/malkuth/actions/workflows/checks.yml) [![Docs](https://img.shields.io/badge/docs-malkuth.docs.celestia.world-blue)](https://malkuth.docs.celestia.world)

</div>

<div align="center">

[English](../en/README.md) · [简体中文](../zhs/README.md) · [繁體中文](../zht/README.md) · [日本語](../ja/README.md) · [한국어](../ko/README.md) · **Français** · [Español](../es/README.md) · [Русский](../ru/README.md) · [العربية](../ar/README.md)

</div>

> **Version 0.1.0** — Crate unique, **basé sur tokio**. Le CLI enveloppe
> *n'importe quel* programme (même un qui n'utilise pas la bibliothèque) avec un pool de pods et un
> proxy inverse persistant.

Malkuth aide les programmes automatisés et de longue durée à accomplir quatre choses difficiles :

1. **Transport enfichable** — JSON-RPC sur boucle locale TCP,
   **WebSocket** distant ou **IPC** local (sockets Unix / tubes nommés via
   [`interprocess`](https://crates.io/crates/interprocess)). Un seul trait
   `Transport`, distribué selon le schéma d'URL.
2. **Basé sur tokio, léger en frameworks** — construit sur `tokio` ; le chemin JSON-RPC ne nécessite
   aucun framework HTTP (axum est optionnel, pour les sondes HTTP uniquement).
3. **Fonctionnalités optionnelles et raccordables** — source de sortie, sondes, hooks de pulsation et de drainage
   sont des *traits*. Utilisez les valeurs par défaut (sortie par signal OS, sondes axum, workers
   supervisés) ou fournissez les vôtres (par ex. déclencher le drainage depuis une commande « stop » in-band
   reçue par votre serveur). Un orchestrateur `Supervised` « piles incluses » les câble
   ensemble.
4. **Un CLI watchdog** — `malkuth -- <cmd>` enveloppe un programme avec surveillance de fichiers, un
   pool de pods et un proxy inverse persistant de couche 4.

## En CLI

```
malkuth [--watch PATH]... [--proxy PUBLIC:LO-HI] [--pod-count N] -- <cmd> [args...]
```

Lancez 5 copies parallèles de votre serveur (chacune écoutant sur la variable d'env `PORT` →
elles s'auto-attribuent 3001–3005), précédées d'un proxy persistant sur 3000 :

```bash
malkuth --watch ./src --watch ./res \
        --proxy 3000:3000-3999 --pod-count 5 \
        -- cargo run
```

Le proxy route chaque **IP cliente** vers un backend fixe par hachage cohérent, de sorte qu'un
client continue d'atteindre le même pod jusqu'à ce que celui-ci redémarre ou soit réduit — la
base pour une release grise / un redémarrage progressif. Lors d'un changement de fichier, il draine et
redémarre un pod à la fois.

## En bibliothèque

```toml
[dependencies]
malkuth = "0.1"
# features: tcp (default) | ws | ipc | signals (default) | worker | probes |
#           file-lock | lease | pg-lock | replica | leader-follower | schema | cli
```

```rust
use std::sync::Arc;
use malkuth::{Client, Router, Server, Supervised, Transport};
use malkuth::transport::TcpTransport;
use serde_json::json;

#[tokio::main]
async fn main() -> std::io::Result<()> {
    // Bind once; build a router with the standard lifecycle RPC + your methods.
    let lis = TcpTransport.listen("tcp://127.0.0.1:0").await?;
    let supervised = Supervised::new().signals();           // OS-signal exit source
    let ctrl = supervised.drain_controller();
    let handler = Arc::new(
        Router::new()
            .lifecycle(ctrl, None)                          // Lifecycle.Drain/Status/...
            .route("ping", |_| Box::pin(async { Ok(json!("pong")) })),
    );
    // Race the server against the exit source, then run drain hooks.
    supervised.serve_rpc_listener(lis, handler).await
}
```

Besoin que le drainage soit déclenché par votre propre logique au lieu des signaux ? Implémentez
`malkuth::ExitSource` et passez-le via `.exit(...)`. Vous voulez une coordination sauvegardée par
Postgres ? La fonctionnalité `pg-lock` fournit un backend `CoordinationLock`.

## Drapeaux de fonctionnalité

| Fonctionnalité | Active |
| --- | --- |
| `tcp` *(défaut)* | JSON-RPC sur TCP local/distant (`tokio::net`) |
| `ws` | JSON-RPC sur WebSocket (`tokio-tungstenite`) |
| `ipc` | JSON-RPC sur IPC local (`interprocess`) |
| `signals` *(défaut)* | `ExitSource` par défaut par signal OS (`tokio::signal`) |
| `worker` | Workers supervisés en processus enfant (`tokio::process`) |
| `probes` | Routeur axum `/healthz` + `/readyz` |
| `file-lock` | Backend `CoordinationLock` POSIX `flock` (unix) |
| `lease` | `CoordinationLock` par bail de fichier avec expiration automatique TTL (résistant aux crashs) |
| `pg-lock` | Backend PostgreSQL `pg_advisory_lock` (`tokio-postgres`) |
| `replica` | `InstanceRegistry` en mémoire |
| `leader-follower` | `LeaseLeaderElector` (sur le backend de bail) |
| `schema` | Dérivations `schemars::JsonSchema` pour les types de fil |
| `cli` | Le binaire watchdog `malkuth` (pool de pods + proxy persistant) |

## Statut

Les couches 1–3 (cycle de vie/drainage, sondes, transfert d'écouteur) et le cœur JSON-RPC
(codec + serveur/client + transports tcp/ws/ipc) sont implémentés et testés
de bout en bout. Le pool de pods du CLI + proxy persistant fonctionne (vérifié e2e). Les trois
backends `CoordinationLock` (`file-lock`, `lease`, `pg-lock`) et le
`LeaseLeaderElector` `leader-follower` sont implémentés. Voir
[docs/design/](../en/design/) pour la conception.

## Licence

[Synthetic Source License 1.0 (SySL)](../../LICENSE) — une licence de l'ère de l'IA qui opère
comme un contrat contraignant indépendant du statut de droit d'auteur.
