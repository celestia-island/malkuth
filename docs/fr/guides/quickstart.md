# Démarrage rapide

## Ajouter la dépendance

```toml
[dependencies]
malkuth = { git = "https://github.com/celestia-island/malkuth.git", branch = "dev" }
# features: tcp (default) | ws | ipc | signals (default) | worker | probes |
#           file-lock | lease | pg-lock | replica | leader-follower | schema
```

## Un service JSON-RPC minimal

```rust
use std::sync::Arc;
use malkuth::{Router, Supervised};
use malkuth::transport::TcpTransport;
use malkuth::Transport;
use serde_json::json;

#[tokio::main]
async fn main() -> std::io::Result<()> {
    let lis = TcpTransport.listen("tcp://127.0.0.1:0").await?;
    let supervised = Supervised::new().signals();
    let ctrl = supervised.drain_controller();

    let handler = Arc::new(
        Router::new()
            .lifecycle(ctrl, None)            // registers Lifecycle.Drain / Status / Health / Reload
            .route("ping", |_| Box::pin(async { Ok(json!("pong")) })),
    );

    supervised.serve_rpc_listener(lis, handler).await
}
```

`Supervised` fait courir le serveur JSON-RPC contre la source de sortie par
signal OS (SIGINT/SIGTERM → vidange, SIGHUP → rechargement, SIGQUIT → sortie
immédiate), puis exécute les hooks de vidange enregistrés. Remplacez
`.signals()` par `.exit(your_impl)` pour déclencher la vidange depuis votre
propre logique.

## Appel depuis un client

```rust
use malkuth::Client;
use malkuth::transport::TcpTransport;
use malkuth::Transport;
use serde_json::json;

let mut c = Client::connect(&TcpTransport, "tcp://127.0.0.1:8080").await?;

// Custom method:
let r = c.call("ping", json!({})).await?;       // → "pong"

// Standard lifecycle methods (registered by Router::lifecycle):
c.notify("Lifecycle.Drain", json!({})).await?;  // → server begins graceful drain
let health = c.call("Lifecycle.Health", json!({})).await?;
// → { "alive": true, "pid": 12345, "uptime_secs": 360, "version": "0.1.0" }
let status = c.call("Lifecycle.Status", json!({})).await?;
// → { "ready": true, "draining": false, "dependencies": [], "generation": null }
```

## Le protocole de cycle de vie JSON-RPC

`Router::lifecycle(drain, probe)` enregistre quatre méthodes standard :

| Méthode | Paramètres | Résultat | Effet |
| --- | --- | --- | --- |
| `Lifecycle.Drain` | `{}` | `{ "accepted": true, "draining": true }` | Démarre la vidange gracieuse |
| `Lifecycle.Reload` | `{}` | `null` | Démarre le rechargement (sans sortie) |
| `Lifecycle.Status` | `{}` | `ReadyStatus` | Interroge la disponibilité (bit de vidange + dépendances) |
| `Lifecycle.Health` | `{}` | `HealthStatus` | Interroge la vivacité (pid / uptime / version) |

Tous les messages sont du JSON-RPC 2.0 encapsulé NDJSON sur le transport choisi.

## Autres transports

Remplacez `TcpTransport` par `WsTransport` (feature `ws`, adresse `ws://host:port`)
ou `IpcTransport` (feature `ipc`, adresse `ipc:/tmp/sock`). Ou utilisez
`MultiTransport`, qui répartit selon le schéma d'URL (`tcp://` / `ws://` / `ipc:`).

## Envelopper n'importe quel programme avec le CLI

```bash
malkuth --watch ./src --proxy 3000:3000-3999 --pod-count 3 -- cargo run
```

Cela lance 3 pods (auto-attribution des ports 3001–3003 via la variable
d'environnement `PORT`), sonde chacun jusqu'à ce qu'il écoute, et les frontale
avec un reverse-proxy persistant sur le port 3000 (routage par hachage cohérent
selon l'IP du client). Une modification sous `./src` déclenche un redémarrage
progressif, un pod à la fois.
