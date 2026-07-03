# Architecture unifiée de supervision, de mise à jour progressive et de réplication
> **Périmètre.** Ceci est un design de *niveau plateforme* : il traverse `core`
> (entelecheia / scepter), `webui` (shittim-chest / chest) et `router`
> (evernight). Les documents d'architecture par projet se trouvent dans leurs
> propres sous-catégories `core/`, `webui/`, `router/` ; le présent document
> définit la couche de cycle de vie / supervision partagée que tous consomment.
## 1. Contexte et objectifs
Les trois projets sont structurellement homogènes — tous en **Rust (edition
2024, MSRV 1.85) + axum 0.8 + tokio + JSON-RPC sur sockets Unix /
WebSocket**, et tous partagent déjà la crate `arona` comme couche de
protocole. C'est cette homogénéité qui rend pertinent le fait de construire
*une seule* fois un mécanisme de supervision et de le réutiliser trois fois.
Le mécanisme doit servir quatre besoins imbriqués, exprimés comme une seule
fonctionnalité cohérente :
1. **Équilibrage de charge** — exécuter plusieurs instances identiques d'un
   même programme afin qu'elles se répartissent le travail, en se coordonnant
   par IPC, tout en partageant la base de données / la configuration / l'état
   d'exécution.
2. **Écritures coordonnées** — lorsqu'une instance s'apprête à écrire un
   fichier partagé, elle doit notifier les autres et prendre un verrou, afin
   que des mutations concurrentes ne corrompent pas l'état.
3. **Mises à jour progressives** — lorsqu'une nouvelle version officielle (ou
   un serveur de débogage fraîchement compilé localement) arrive, les nouveaux
   et anciens binaires peuvent coexister ; l'ancien processus termine son
   travail en cours, puis se termine et passe la main au nouveau processus.
4. **Tolérance aux pannes en périphérie** — sur un appareil (notamment les
   passerelles evernight), deux processus peuvent s'exécuter en leader/follower
   afin que le plantage de l'un ne fasse pas tomber tout l'appareil.
### 1.1 État actuel (le manque que cela comble)
Un audit de code a révélé les mêmes trois lacunes dans **les trois**
projets :
| Capacité | entelecheia (scepter) | shittim-chest (chest) | evernight |
|---|---|---|---|
| Traitement des signaux | `ctrl_c` uniquement (`shutdown.rs:17`) | `ctrl_c` uniquement (`api.rs:465`) | `ctrl_c` uniquement (`api/mod.rs:109`) |
| Logique de vidange | vidange HTTP, pas WS / tâches de fond | identique | aucune |
| Passage de fd de listener | aucune | aucune | aucune |
| `/readyz` avec bit de vidange | aucune | a `/api/health`, pas de bit de vidange | aucune |
Le problème central : comme seul `SIGINT` est intercepté, `docker stop` /
`systemctl restart` — qui envoient **`SIGTERM`** — contournent totalement
l'arrêt gracieux et tuent brutalement après le délai de grâce. Corriger ce
seul point est le changement au plus fort levier.
### 1.2 Actifs existants à réutiliser
- `entelecheia/packages/cli/src/evernight_daemon.rs` — le blueprint
  d'auto-redémarrage le plus abouti à ce jour : fichier de verrouillage PID +
  ré-exécution de soi-même + attente de disponibilité + repli
  `SIGTERM`→`SIGKILL`.
- `entelecheia/packages/scepter/src/daemon/health_daemon.rs` — une **file
  d'attente de fichiers manifest JSON** qui réalise des mises à jour
  progressives de conteneurs (une primitive de mise à jour indépendante du
  langage).
- `entelecheia/packages/shared/infra_jsonrpc` — la couche de transport
  JSON-RPC sur socket Unix.
- `shittim-chest/packages/core/src/proxy/upstream_pool.rs` — un registre
  multi-endpoints avec reconnexion à recul exponentiel ; un modèle prêt à
  l'emploi pour un client à équilibrage de charge.
- `evernight/src/model_server.rs` — une machine à états de cycle de vie de
  ressource `Running/Starting/Stopped/Failed` avec
  déployer→attendre-santé→arrêter-l'ancien ; le modèle interne au dépôt pour
  la mise à jour progressive au niveau applicatif.
## 2. Bases théoriques
Le mécanisme n'est pas une théorie unique mais une composition de motifs
industriels bien établis, chacun avec des implémentations de référence. Ils
sont nommés explicitement afin que le design hérite de leurs sémantiques
éprouvées :
| Besoin exprimé | Terme industriel | Implémentation de référence |
|---|---|---|
| Nouveaux et anciens processus coexistent ; l'ancien termine son travail en cours puis se termine | **Arrêt gracieux / vidange (drain)** + **mise à jour progressive** | Kubernetes Deployment, nginx / unicorn |
| « Marqueur rouge/bleu » (nom rappelé en discussion) | **Déploiement bleu-vert** (deux environnements parallèles, on bascule le pointeur de trafic). Le comportement de dégradation progressive décrit ressemble davantage à **mise à jour progressive + drain** qu'au bleu-vert. | |
| Un nouveau processus prend la main sur le même port sans perdre de connexions | **activation de socket / héritage de fd / `SO_REUSEPORT`** | systemd, `USR2` de nginx, redémarrage à chaud d'envoy |
| « Notifier l'autre et verrouiller avant d'écrire » | **verrou consultatif (`flock`/`fcntl`)/ verrou de ligne en base / bail (lease)** | verrous consultatifs POSIX, `pg_advisory_lock` |
| Auto-réparation des processus | **arbre de supervision (OTP) / systemd / kubelet** | Erlang/OTP, systemd, s6, immortal |
| Leader/follower pour qu'un plantage ne tue pas le service | **élection de leader par bail + fencing** | bail Chubby, élection de leader Raft (sous-ensemble), keepalived/VRRP, Pacemaker |
| Ressource détenue par un processus, redémarrée en cas de plantage | **« let it crash » + redémarrage par superviseur** | arbres de supervision Erlang/OTP, systemd `Restart=always` |
**La recette standard industrielle de mise à jour progressive** (à recopier
telle quelle) est celle de Kubernetes :
`maxSurge + maxUnavailable + readinessProbe + preStop hook + SIGTERM
arrêt gracieux + délai de grâce + PodDisruptionBudget`. Ce que nous
construisons est la variante auto-hébergée, mono-hôte / petit cluster de
cette recette.
**La mise à niveau à chaud de nginx** est la référence pour « nouveau et
ancien coexistent, puis l'ancien se vide » : `USR2` démarre un nouveau master
qui hérite du fd d'écoute → `WINCH` arrête gracieusement les anciens workers
→ `QUIT` retire l'ancien master. Le flux du §7 est structurellement identique.
## 3. Architecture d'ensemble
Le design converge vers une ossature unique et élégante : **un arbre de
supervision partout ; la seule différence est de savoir si le superviseur est
un réplica pair ou un leader/follower.**
```
                     ┌─────────────────────────────────────────┐
    Base partagée    │ sémantique des signaux · vidange        │  ← identique pour
    (Layer 1 / 3)    │ /healthz · /readyz (avec bit drain)     │     les trois projets
                     │ socket activation + 3 adaptateurs déploiement │
                     └─────────────────────────────────────────┘
                                   │
           ┌───────────────────────┴───────────────────────────┐
           ▼                                                  ▼
    ┌────────────────┐                              ┌──────────────────┐
    │ Sous-système A │  superviseurs en PAIRS       │ Sous-système B   │  superviseur est
    │ Réplica        │  (actif-actif)               │ Leader/Follower  │  leader ou follower
    │                │  ← LB serveur + màj progressive │                  │  ← tolérance aux pannes périphérie
    └───────┬────────┘                              └─────────┬────────┘
            │                                                  │
            └──────────────────┬───────────────────────────────┘
                               ▼
                ┌──────────────────────────────┐
                │ Abstraction Worker unifiée   │  ← chaque « ressource-processus-enfant »
                │ FSM de cycle de vie +        │     pour les trois projets
                │ redémarrage supervisé        │     cosmos / pglite-proxy / workers par protocole
                │ (permanent/transient)        │
                │ + limite de débit fenêtre glissante │
                └──────────────────────────────┘
```
### 3.1 La simplification clé : la mise à jour progressive *n'est pas* un troisième système
Une fois l'arbre de supervision devenu l'ossature, les besoins de
l'utilisateur se ramènent à **deux** sous-systèmes, pas trois :
- **Sous-système A — Réplica.** Le côté serveur exécute N instances pairs
  identiques qui se répartissent la charge. *L'équilibrage de charge et la
  mise à jour progressive sont le même sous-système :* la mise à jour
  progressive se résume à « nombre de réplicas temporairement +1 → vider un
  ancien réplica → répéter ». C'est l'opération `maxSurge/maxUnavailable` de
  Kubernetes, exprimée sous forme d'ajout/retrait de réplica.
- **Sous-système B — Leader/Follower.** La périphérie (appareil evernight)
  exécute deux processus, un leader et un follower, pour la tolérance aux
  pannes. Le leader détient exclusivement les E/S physiques ; le follower
  attend.
Il n'y a pas de « système de mise à jour progressive » séparé : c'est une
opération de maintenance du sous-système A (et, avec une sémantique
différente, de B via le failover de leader).
### 3.2 Vue en couches
| Couche | Sous-système A (Réplica) | Sous-système B (Leader/Follower) | Partagé ? |
|---|---|---|---|
| **L1** cycle de vie (signaux / drain / sondes) | identique | identique | **partagé** |
| **L3** passation sans interruption (socket activation) | chaque réplica reçoit le fd de systemd | prise de fd leader→follower (avancé) | **partagé** |
| **L2** coordination | **2a** registre de pairs + verrou partagé (`pg_advisory`) | **2b** élection par bail + ressource exclusive + registre leader/follower | **diverge** (même trait, politique différente) |
| **L4** orchestration | **4a** mise à l'échelle de réplica / mise à jour progressive | **4b** failover | **diverge** |
Constat : **L1 et L3 sont entièrement partagés ; L2/L4 divergent.**
`CoordinationLock` est le même trait en 2a et 2b — utilisé pour coordonner
des écritures concurrentes dans A, utilisé comme bail de leader dans B. Cette
unification du trait est précisément là qu'« atterrissent » les principes
communs.
## 4. Appropriation des crates
L'objectif utilisateur « mettre ça dans arona » doit être scindé, car **arona
aujourd'hui est une crate purement de protocole/types** — uniquement des
dépendances `serde` / `ts-rs` / `schemars`, `lib.rs:5` impose « chaque type
est défini dans entelecheia et consommé par shittim-chest », et il
`exclude` tous les artefacts non-protocole pour la publication crates.io.
Injecter de la logique d'exécution (tokio, `sd_listen_fds`, traitement des
signaux) casserait cette identité légère et publiable.
Découpage :
- **`arona::lifecycle` (contrat de protocole, vit dans arona).** Uniquement
  des méthodes et types JSON-RPC : `DrainState`, `ReadyStatus`,
  `Lifecycle.Drain`, `Lifecycle.Status`, `Worker.Status`, etc. Satisfait la
  règle d'arona « apparié des deux côtés ».
- **`malkuth` (nouvelle crate, exécution).** Dépend des types de
  protocole `arona` + `tokio` + une liaison `libsystemd` (socket activation)
  + des traits de backend. Activé par fonctionnalités (feature-gated) :
  - `replica` — coordination + orchestration du sous-système A.
  - `leader-follower` — élection par bail + failover du sous-système B.
  - `socket-activation` — acquisition de fd systemd.
  - `file-lock` / `pg-lock` / `lease` — backends `CoordinationLock`.
Les trois projets dépendent de `malkuth` et activent les
fonctionnalités dont ils ont besoin (voir la matrice §8). Tout mettre dans
arona le forcerait à devenir « protocole + exécution optionnelle » et
détruirait sa pureté — non recommandé.
## 5. Abstractions principales
### 5.1 `Worker` — une ressource-processus-enfant supervisée
Un `Worker` est un processus tuable indépendamment qui détient exactement une
ressource (une connexion PLC, un port série, un port d'écoute local, un
sidecar comme cosmos / pglite-proxy). Le processus est la **frontière
d'isolation des pannes** : un bug dans la pile Modbus ne peut pas corrompre
le worker S7comm.
FSM de cycle de vie (tiré de `evernight/src/model_server.rs:128-139`) :
```
        démarrage                  santé ok
  Starting ──────► Running ─────────────────► Running
      │              │  ▲
      │              │  │ santé ok (auto-réparation)
      │              ▼  │
      └──────► Failed ◄┘        plantage / mauvaise santé
                   │
                   │ politique de redémarrage = permanent
                   └────────► Starting (limité en débit)
```
### 5.2 `Supervisor` — détient le pool de workers
- **Politique de redémarrage** (vocabulaire OTP) : `permanent` (toujours
  redémarrer — valeur par défaut pour les workers de ressource),
  `transient` (redémarrer uniquement sur sortie anormale), `temporary` (ne
  jamais redémarrer).
- **Limitation de débit par fenêtre glissante** (tirée du `health_daemon`
  d'entelecheia `max_restart_attempts` + `cooldown`) : si un worker
  redémarre plus de N fois dans la fenêtre W, il entre en `cooldown` pour
  éviter les tempêtes de plantage ; les redémarrages ultérieurs sont
  différés.
### 5.3 `Lifecycle` — sémantique de signaux uniforme (Layer 1)
Adopter la convention nginx/Go :
| Signal | Sémantique | Comportement |
|---|---|---|
| `SIGINT` (ctrl_c) | équivalent à SIGTERM (convivial pour le dev) | entrer en vidange |
| `SIGTERM` | **arrêt gracieux** | vidange : effacer le bit ready → arrêter l'acceptation → vider l'en cours → sortir |
| `SIGHUP` | **rechargement à chaud de la configuration** | ne pas sortir ; relire la configuration |
| `SIGQUIT` | **sortie immédiate** (urgence uniquement) | ignorer la vidange, sortie rapide |
**Séquence de vidange** (une implémentation ; chaque projet injecte sa
propre « fermeture de vidange ») :
1. Placer `/readyz` `draining = true` (le LB / l'orchestrateur le voit et
   cesse d'envoyer du nouveau trafic).
2. Arrêter l'`accept` sur les nouvelles connexions (en socket activation :
   arrêter l'acceptation depuis le fd hérité).
3. Envoyer des trames de fermeture aux WebSockets actives ; attendre les
   requêtes en cours avec un délai `DRAIN_TIMEOUT` (30s par défaut,
   configurable).
4. Vider les tâches de fond (copier `TaskManager.stop_all` +
   `wait_all` d'entelecheia).
5. Déconnecter proprement les pools en amont (copier la déconnexion propre
   du `upstream_pool` de shittim-chest).
6. Relâcher les verrous, nettoyer les fichiers temporaires → exit 0.
Note d'implémentation : `axum::serve(listener,
app).with_graceful_shutdown(...)` d'axum prend déjà en charge la vidange ;
**la pièce manquante clé est de câbler `SIGTERM`** (aujourd'hui seul `ctrl_c`
est câblé). Références : `entelecheia/.../shutdown.rs:17`,
`shittim-chest/.../api.rs:465`, `evernight/src/api/mod.rs:109`.
### 5.4 Endpoints de santé (uniformes)
Séparer les sondes (aujourd'hui les trois projets sont incohérents) :
| Endpoint | Sémantique | Décision |
|---|---|---|
| `/healthz` (liveness) | processus en vie | 200 si le processus peut répondre (critère simple de redémarrage) |
| `/readyz` (readiness) | **peut servir**, porte le bit de vidange | 200 si pas en cours de vidange ET dépendances disponibles (ping DB / socket scepter / premier poll de station) ; 503 pendant la vidange |
Le bit `draining` de `/readyz` est le signal central de mise à jour
progressive : l'orchestrateur route les nouvelles requêtes uniquement vers
les instances dont `/readyz` est 200. Le `GET /api/health` existant de
shittim-chest (`routes.rs:27`) est mis à niveau vers `/readyz` avec un bit
de vidange.
### 5.5 `acquire_listener` — passation sans interruption de Layer 3
`malkuth` expose `acquire_listener(addr) -> TcpListener` :
1. Essayer `sd_listen_fds()` (valider `LISTEN_PID`) — systemd détient le fd.
2. Repli sur un simple `TcpListener::bind(addr)` (dev, sans systemd).
axum `serve(listener, ...)` accepte déjà un listener pré-lié, donc la
plomberie existe ; seule la *source* du fd manque aujourd'hui. Trois
adaptateurs de déploiement :
| Déploiement | Approche | S'applique à |
|---|---|---|
| **systemd nu** | `xxx.socket` + instances modèles `xxx@.service` | scepter, evernight-gateway, malkuth lui-même |
| **docker** (prod shittim-chest) | socket activation systemd de l'hôte, passer la socket/fd liée dans le conteneur (`LISTEN_FDS` + `SocketUser`) ; ou un master léger dans le conteneur détenant le fd | shittim-chest prod |
| **dev** | repli sur un simple `bind` + bref chevauchement (accepter quelques centaines de ms de connexions perdues), sans systemd | dev des trois projets |
Mise à jour progressive en socket activation :
```
[déclencheur de mise à niveau] → démarrer nouvelle instance (service@new à partir du modèle, hérite/relit le fd)
                               → interroger /readyz de la nouvelle instance jusqu'à 200
                               → SIGTERM l'ancienne instance (= vidange)
                               → l'ancienne instance se vide et se termine elle-même
                               → systemd détient le fd tout au long → zéro connexion perdue
```
### 5.6 `CoordinationLock` — trait de Layer 2 avec backends
Pendant la fenêtre de mise à jour progressive, les anciennes et nouvelles
instances peuvent lire/écrire concurremment des ressources partagées. Les
transactions de base de données sont naturellement sûres ; les fichiers
(JSONL d'evernight, configurations) nécessitent « notifier-avant-écriture +
verrou ».
```rust
pub trait CoordinationLock: Send + Sync {
    async fn acquire(&self, key: &str, lease: Duration) -> Result<LockGuard>;
}
// Backends :
//   FileLock  — flock/fcntl,       pour evernight (JSONL / config)
//   PgLock    — pg_advisory_lock,  pour entelecheia / shittim-chest
//   LeaseLock — verrou fichier + bail (expiration auto en cas de plantage)
```
**Registre d'instances** (utilisé uniquement pendant la fenêtre de mise à
niveau ; un seul enregistrement en régime établi) : une petite table/fichier
partagé enregistrant `{instance_id, role: Active | Draining, started_at,
generation}`. La nouvelle instance écrit une ligne `Active` au démarrage ;
lors d'une mise à niveau, l'ancienne est marquée `Draining`. Cela remplace
le quorum Raft qui était explicitement hors périmètre — car le régime établi
est à instance unique, le registre ne fait que coordonner la vidange, sans
besoin de forte cohérence (un fichier ou une ligne en base suffit).
## 6. Sous-système A — Réplica (équilibrage de charge ⊃ mise à jour progressive)
**Forme.** N instances pairs identiques s'exécutant en parallèle derrière un
LB frontal, l'état dans un Postgres partagé. **actif-actif**, les pairs sont
égaux, pas de leader.
| Préoccupation | Approche |
|---|---|
| Routage des requêtes | LB frontal (caddy / `SO_REUSEPORT` intégré en round-robin) ; retrait par `/readyz` |
| Lecture/écriture d'état partagé | transactions de base de données + `pg_advisory_lock` pour la coordination des écritures concurrentes (naturellement sûr) |
| WebSocket / connexions longues | **session collante** (le LB épingle par cookie/id d'instance) ou **migration de session** (le client se reconnecte à n'importe quel réplica après déconnexion ; l'état est récupéré depuis la base — le `upstream_pool` de shittim-chest est déjà un modèle de reconnexion) |
| Affinité de session | entelecheia et shittim-chest externalisent tous deux l'état vers Postgres et récupèrent au démarrage → **naturellement compatibles réplica**, leur avantage clé sur evernight |
| Mise à jour progressive | sous-opération de mise à l'échelle de réplica : ajouter un nouveau réplica (nouvelle version) → prêt → vider et retirer l'ancien réplica → répéter. `maxSurge/maxUnavailable` de K8s en miniature |
**Pourquoi A est relativement facile.** Comme entelecheia et shittim-chest
externalisent l'état vers Postgres (récupération au démarrage, confirmé par
audit), les réplicas n'ont pas besoin de réplication d'état entre eux — un
LB frontal plus des transactions en base suffit. Le seul point difficile est
le collage / la migration des WebSockets.
## 7. Sous-système B — Leader/Follower (HA périphérie actif-passif)
**Forme.** Deux processus evernight sur le même appareil/passerelle, un
leader un follower ; l'`evernight-server` en amont voit **un seul `node_id`**
(un seul appareil). **actif-passif**, les instances ne sont pas égales, le
leader détient exclusivement les E/S physiques.
### 7.1 L'« astuce » qui simplifie B : arbre de supervision + let-it-crash
Plutôt que de rendre chaque ressource tolérante aux pannes, **seul le
superviseur est rendu leader/follower** ; les ressources sont des processus
worker indépendants qui sont simplement redémarrés en cas de plantage.
Concrètement (selon la décision convenue) :
```
supervisor  (HA leader / follower)         ← seule cette couche fait l'élection par bail + le failover
   ├─ worker : PLC-A (Modbus)              ← redémarrage supervisé, instance unique
   ├─ worker : PLC-B (S7comm)              ← redémarrage supervisé, instance unique
   ├─ worker : série / CAN                 ← redémarrage supervisé, instance unique
   └─ worker : listener port local         ← redémarrage supervisé, instance unique
```
C'est le modèle d'arbre de supervision OTP / « let it crash » (aussi systemd
`Restart=always`, Pod K8s). Bénéfices :
- **Séparation des préoccupations.** Les workers ne font que « détenir une
  ressource et travailler » ; ils ne portent aucune logique de tolérance aux
  pannes, d'élection ou de synchronisation d'état — ils restent
  maximalement simples.
- **Tolérance aux pannes concentrée.** Seul le superviseur fait de la HA ;
  la complexité se ramasse en un seul endroit.
- **Isolation des pannes.** Un plantage dans une ressource (par ex. un
  protocole PLC) ne peut pas affecter les autres (processus séparés).
- **Adéquation aux protocoles.** evernight parle de nombreux protocoles
  industriels (Modbus/S7/CAN/série) ; mapper chacun sur un worker donne une
  valeur d'isolation maximale.
### 7.2 Cycle de vie des workers au failover — modèle en processus-enfant (point de départ convenu)
Les workers sont des **processus enfants** du superviseur (`kill_on_drop`).
Mort du leader → les workers sont rendus orphelins/tués → le follower
promu **relance tous les workers** (chaque PLC se reconnecte).
- Modèle le plus simple ; correspond à l'intention « le serveur est
  responsable ».
- Coût : lors du failover du superviseur, toutes les ressources tombent
  brièvement et se reconnectent.
- Avancé (différé) : workers comme démons indépendants sous un init plus
  bas, le superviseur ne fait que les diriger via IPC ; le nouveau
  superviseur s'`attach`e aux workers survivants. Moins d'interruption, mais
  les workers doivent implémenter « se réattacher au superviseur courant » —
  plus complexe. Documenté comme option future.
### 7.3 Élection du leader + fencing
- **Élection par bail.** Le leader détient un verrou fichier + bail (avec
  TTL), renouvelle à chaque battement de cœur ; le follower interroge ; sur
  dépassement du battement de cœur du leader, le follower s'empare du bail et
  se promeut.
- **Ressource physique exclusive.** Les connexions PLC/série/CAN ne peuvent
  être détenues que par le leader (deux processus interrogeant le même PLC
  entrent en conflit) → le leader interroge, le follower attend. C'est la
  raison fondamentale pour laquelle B est actif-passif, et non actif-actif.
- **Synchronisation d'état.** **Veille froide** comme point de départ (le
  follower ne réplique pas ; lors de la promotion, il récupère depuis le
  JSONL sur disque). La veille chaude (le follower suit le JSONL du leader)
  est une option avancée.
- **Fencing anti-split-brain.** TTL du bail + fencing : le follower ne peut
  s'emparer du bail qu'après son expiration réelle ; après prise de contrôle,
  l'ancien leader est physiquement empêché d'écrire davantage (l'exclusivité
  des E/S physiques est un fence naturel).
- **Identité d'appareil unique.** Leader et follower partagent un seul
  `node_id` ; seul le leader courant émet `device.register`.
Analoga classiques : keepalived/VRRP, DRBD+Pacemaker, MySQL
primary/replica, Redis Sentinel — comme une simplification dans la machine,
au niveau processus. La théorie (élection par bail + fencing) est bien
fondée.
## 8. Matrice d'adoption par projet
| Projet | rôle du superviseur | workers | stratégie |
|---|---|---|---|
| entelecheia (scepter) | un des réplicas | sidecar cosmos, conteneurs d'agents | **A Réplica** |
| shittim-chest (chest) | un des réplicas | pglite-proxy (mock), channel intake | **A Réplica** |
| appareil evernight (`sensor-poll`) | **leader / follower** | un worker par protocole (Modbus/S7/CAN/série) | **B Leader/Follower** |
| evernight-server (central) | un des réplicas | conteneurs model_server | **A Réplica** |
Sélection des fonctionnalités par projet (`malkuth`) :
- entelecheia / shittim-chest / evernight-server : `replica` +
  `socket-activation` + `pg-lock` ; abstraction worker pour leurs sidecars.
- appareil evernight : `leader-follower` + `socket-activation` (prise de fd
  avancée) + `file-lock` / `lease` ; abstraction worker pour les processus
  par protocole.
## 9. Phases de déploiement
1. **Phase A — Layer 1 sur les trois projets.** Sémantique des signaux +
   `/healthz` / `/readyz` + vidange. Risque le plus bas, gain immédiat le
   plus élevé (corrige d'abord le kill brutal de SIGTERM).
2. **Phase B — protocole `arona::lifecycle` + squelette
   `malkuth`.** Définitions de traits, `acquire_listener`,
   trait `CoordinationLock` + backends `FileLock` / `PgLock`, primitives
   `Worker` + `Supervisor`.
3. **Phase C — Layer 3.** Unités de socket activation pour les trois
   projets + l'adaptateur docker + le repli dev.
4. **Phase D — Layer 4.** Orchestrateur à file de manifest (copier
   `health_daemon`) + boucle de vidange de coexistence ancien/nouveau, y
   compris le flux dev « compiler un nouveau serveur » ; plus le failover
   leader/follower pour B.
## 10. Risques et limites
- **docker + socket activation de shittim-chest** est la pièce la plus
  incertaine ; la passation de fd dans le conteneur nécessite un spike de
  prototype. Si infaisable, repli sur « caddy externe + retrait `/readyz` +
  bref chevauchement ».
- **L'état en mémoire d'evernight** (`DeviceRegistry`, sessions) est perdu à
  la vidange ; décider s'il faut le persister ou le migrer (pire cas : la
  nouvelle instance reconstruit, les sessions longues tombent).
- **La vidange WebSocket ≠ migration.** Le WS en cours tombe quand même quand
  l'ancienne instance se termine ; « sans interruption » exige que le client
  se reconnecte à une nouvelle instance (le client de shittim-chest a déjà
  une logique de reconnexion `upstream_pool`, réutilisable).
- **Tolérance aux pannes au niveau processus vs niveau thread.** Le
  leader/follower (sous-système B) résout la tolérance aux pannes au
  *niveau processus/instance*, pas au niveau thread. Un panic tokio dans une
  tâche relève du superviseur (redémarrage de tâche, pas failover de
  processus). N'utilisez pas B pour absorber des plantages de thread — ce
  serait beaucoup trop lourd.
- **Explicitement hors périmètre.** Quorum Raft, sharding par hachage
  cohérent, HA inter-datacenter — exclus par le périmètre « mise à jour
  progressive + HA périphérie ». Le régime établi à instance unique signifie
  que le registre ne coordonne que la vidange.
## 11. Questions ouvertes (différées)
- Workers au failover : modèle en processus-enfant choisi comme point de
  départ ; le modèle « démon indépendant + attach » est documenté comme
  avancé.
- Veille froide vs chaude pour B : froide choisie comme point de départ
  (récupération depuis le JSONL lors de la promotion) ; chaude (le follower
  suit le journal du leader) différée.
- S'il faut aussi englober le sidecar `cosmos` d'entelecheia et le
  `pglite-proxy` de shittim-chest sous l'abstraction `Worker` unifiée :
  oui convenu (unifier dans une seule abstraction worker pour les trois
  projets).
---
*Source canonique en anglais :
`docs/en/design/platform/supervision-and-rolling-update.md`. Les autres
langues (zht/ja/ko/es/ru) sont en attente d'i18n.*
