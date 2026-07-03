# Transfert du listener

## Le problème

Lorsqu'un processus serveur redémarre, il existe une fenêtre pendant laquelle personne n'écoute
sur le port — les connexions entrantes sont perdues. Pour des mises à jour tournantes sans
interruption, le nouveau processus doit hériter le socket d'écoute de l'ancien.

## La solution : socket activation

systemd (ou un lanceur personnalisé) maintient le fd du socket d'écoute ouvert. Lorsque le
processus redémarre, le nouveau processus hérite le fd et peut immédiatement accepter des
connexions — le noyau les met en file d'attente pendant l'intervalle.

Le `acquire_listener` de Malkuth implémente ceci en **Rust pur** (sans `libsystemd`) :

```rust
use malkuth::acquire_listener;

// Prefers systemd socket activation (fd inherited), falls back to a plain bind.
let listener = acquire_listener("0.0.0.0:8080").await?;
```

Activez la fonctionnalité `socket-activation` :

```toml
[dependencies]
malkuth = { git = "https://github.com/celestia-island/malkuth.git", branch = "dev", features = ["socket-activation"] }
```

## Comment ça marche

systemd définit deux variables d'environnement :

| Variable | Signification |
| --- | --- |
| `LISTEN_PID` | PID du processus qui doit hériter les fds (doit être le nôtre) |
| `LISTEN_FDS` | Nombre de fds transmis (à partir de fd 3) |

Malkuth les lit, valide que `LISTEN_PID == our_pid`, prend possession de fd 3
(`SD_LISTEN_FDS_START`), le passe en mode non-bloquant et l'enveloppe dans un
`tokio::net::TcpListener`.

Si les variables sont absentes ou le PID ne correspond pas, il retombe sur
`TcpListener::bind(addr)`.

## Exemple d'unité systemd

```ini
# /etc/systemd/system/myapp.socket
[Socket]
ListenStream=8080

[Install]
WantedBy=sockets.target
```

```ini
# /etc/systemd/system/myapp.service
[Service]
ExecStart=/usr/bin/myapp
# systemd passes the socket fd automatically when the socket unit is active
```

Avec cette configuration, `systemctl restart myapp` ne perd aucune connexion en cours :
le noyau les maintient dans la file d'attente d'écoute pendant que le nouveau processus
démarre et hérite le fd.
