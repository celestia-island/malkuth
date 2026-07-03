# Arquitectura Unificada de Supervisión, Rolling Update y Replicación
> **Alcance.** Este es un diseño de *nivel plataforma*: atraviesa `core`
> (entelecheia / scepter), `webui` (shittim-chest / chest) y `router`
> (evernight). Los documentos de arquitectura de cada proyecto viven bajo
> sus propias subcategorías `core/`, `webui/`, `router/`; este documento
> define la capa compartida de ciclo de vida / supervisión que todos
> consumen.
## 1. Antecedentes y Objetivos
Los tres proyectos son estructuralmente homogéneos: todos son **Rust
(edition 2024, MSRV 1.85) + axum 0.8 + tokio + JSON-RPC sobre sockets
Unix / WebSocket**, y todos comparten ya el crate `arona` como capa de
protocolo. Precisamente esta homogeneidad es lo que hace que valga la
pena construir *un único* mecanismo de supervisión y reutilizarlo tres
veces.
El mecanismo debe servir a cuatro necesidades superpuestas, expresadas
como una sola facilidad coherente:
1. **Balanceo de carga** — ejecutar múltiples instancias idénticas del
   mismo programa para que repartan el trabajo, coordinándose por IPC,
   compartiendo a la vez estado de base de datos / configuración /
   runtime.
2. **Escrituras coordinadas** — cuando una instancia va a escribir un
   archivo compartido, debe notificar a las demás y tomar un lock, de
   modo que las mutaciones concurrentes no corrompan el estado.
3. **Rolling updates** — cuando llega un nuevo release oficial (o un
   servidor de depuración recién compilado en local), los binarios nuevo
   y viejo pueden coexistir; el proceso viejo termina su trabajo en
   vuelo, luego sale y transfiere el conjunto en ejecución al proceso
   nuevo.
4. **Tolerancia a fallos de borde** — en un dispositivo (en especial las
   pasarelas evernight) pueden ejecutarse dos procesos como
   leader/follower, de modo que el fallo de uno no tire todo el
   dispositivo.
### 1.1 Estado actual (el hueco que esto cierra)
Una auditoría de código encontró las mismas tres deficiencias en **los
tres** proyectos:
| Capacidad | entelecheia (scepter) | shittim-chest (chest) | evernight |
|---|---|---|---|
| Manejo de señales | sólo `ctrl_c` (`shutdown.rs:17`) | sólo `ctrl_c` (`api.rs:465`) | sólo `ctrl_c` (`api/mod.rs:109`) |
| Lógica de drain | hace drain de HTTP, no de WS / tareas en background | ídem | ninguna |
| Traspaso del fd de escucha | ninguna | ninguna | ninguna |
| `/readyz` con bit de drain | ninguna | tiene `/api/health`, sin bit de drain | ninguna |
El problema principal: como sólo se captura `SIGINT`, los `docker stop`
/ `systemctl restart` — que envían **`SIGTERM`** — eluden por completo
el apagado elegante y matan a la fuerza tras el grace period. Arreglar
sólo esto es el cambio individual de mayor impacto.
### 1.2 Activos existentes para reutilizar
- `entelecheia/packages/cli/src/evernight_daemon.rs` — el blueprint de
  self-restart más completo hoy: PID lockfile + self-reexec + espera de
  readiness + fallback `SIGTERM`→`SIGKILL`.
- `entelecheia/packages/scepter/src/daemon/health_daemon.rs` — una
  **cola de archivos con manifiestos JSON** que realiza rolling updates
  de contenedores (una primitiva de actualización independiente del
  lenguaje).
- `entelecheia/packages/shared/infra_jsonrpc` — la capa de transporte
  JSON-RPC sobre socket Unix.
- `shittim-chest/packages/core/src/proxy/upstream_pool.rs` — un registro
  multi-endpoint con reconexión con backoff exponencial; una plantilla
  lista para un cliente con balanceo de carga.
- `evernight/src/model_server.rs` — una máquina de estados de ciclo de
  vida de recurso `Running/Starting/Stopped/Failed` con
  deploy→esperar-health→parar-el-viejo; la plantilla interna de rolling
  update a nivel de aplicación.
## 2. Fundamento teórico
El mecanismo no es una sola teoría sino una composición de patrones
industriales consolidados, cada uno con implementaciones canónicas. Se
nombran explícitamente para que el diseño herede sus semánticas probadas:
| Necesidad expresada | Término industrial | Implementación canónica |
|---|---|---|
| Procesos nuevo y viejo coexisten; el viejo termina lo en vuelo y luego sale | **Graceful shutdown / drain** + **rolling update** | Kubernetes Deployment, nginx / unicorn |
| "Marcador rojo/azul" (el nombre recordado en la discusión) | **Blue-Green deployment** (dos entornos paralelos, se conmuta el puntero de tráfico). El comportamiento de decaimiento progresivo descrito se asemeja más a **rolling update + drain** que a blue-green. | |
| El proceso nuevo toma el mismo puerto sin soltar conexiones | **socket activation / fd inheritance / `SO_REUSEPORT`** | systemd, nginx `USR2`, envoy hot restart |
| "Notificar al otro y bloquear antes de escribir" | **advisory lock (`flock`/`fcntl`)/ DB row lock / lease** | POSIX advisory locks, `pg_advisory_lock` |
| Self-healing del proceso | **supervision tree (OTP)/ systemd / kubelet** | Erlang/OTP, systemd, s6, immortal |
| Leader/follower para que un fallo no mate el servicio | **lease-based leader election + fencing** | Chubby lease, Raft leader election (subconjunto), keepalived/VRRP, Pacemaker |
| Recurso retenido por un proceso, reiniciado ante fallo | **"Let it crash" + supervisor restart** | Erlang/OTP supervision trees, systemd `Restart=always` |
**La receta estándar de la industria para rolling update** (vale
copiarla literal) es la de Kubernetes:
`maxSurge + maxUnavailable + readinessProbe + preStop hook + SIGTERM
graceful shutdown + grace period + PodDisruptionBudget`. Lo que
construimos es la variante self-hosted, de host único / clúster pequeño
de esa receta.
**La hot-upgrade de nginx** es el libro de texto de "nuevo y viejo
coexisten, luego el viejo hace drain": `USR2` arranca un nuevo master
que hereda el fd de escucha → `WINCH` detiene elegantemente los workers
viejos → `QUIT` retira el master viejo. El flujo del §7 es estructuralmente
idéntico.
## 3. Arquitectura general
El diseño converge en una única columna vertebral elegante: **un árbol
de supervisión en todas partes; la única diferencia es si el supervisor
es una réplica par o un leader/follower.**
```
                    ┌─────────────────────────────────────────┐
   Base compartida  │ semántica de señales · drain             │  ← idéntico para
   (Layer 1 / 3)    │ /healthz · /readyz (con bit de drain)    │     los tres proyectos
                    │ socket activation + 3 adaptadores deploy │
                    └─────────────────────────────────────────┘
                                   │
        ┌──────────────────────────┴──────────────────────────┐
        ▼                                                        ▼
 ┌────────────────┐                              ┌──────────────────┐
 │ Subsistema A   │  supervisores son PARES      │ Subsistema B     │  supervisor es
 │ Réplica        │  (active-active)             │ Leader/Follower  │  leader o follower
 │                │  ← LB servidor + rolling upd.│                  │  ← tolerancia a fallos de edge
 └───────┬────────┘                              └─────────┬────────┘
         │                                                  │
         └──────────────────┬───────────────────────────────┘
                            ▼
             ┌──────────────────────────────┐
             │ Abstracción unificada Worker │  ← todos los "recursos de subproceso"
             │ FSM de ciclo de vida +       │     de los tres proyectos
             │ reinicio supervisado         │     cosmos / pglite-proxy / workers por protocolo
             │ (permanent/transient)        │
             │ + rate limit ventana desliz. │
             └──────────────────────────────┘
```
### 3.1 La simplificación clave: rolling update *no* es un tercer sistema
Una vez que el árbol de supervisión es la columna vertebral, las
necesidades del usuario se colapsan a **dos** subsistemas, no tres:
- **Subsistema A — Réplica.** El lado servidor ejecuta N instancias
  pares idénticas que reparten carga. *El balanceo de carga y el rolling
  update son el mismo subsistema:* el rolling update es simplemente
  "conteo de réplicas temporalmente +1 → hacer drain de una réplica
  vieja → repetir". Es la operación `maxSurge/maxUnavailable` de
  Kubernetes, expresada como suma/eliminación de réplicas.
- **Subsistema B — Leader/Follower.** El borde (dispositivo evernight)
  ejecuta dos procesos, uno leader y uno follower, para tolerancia a
  fallos. El leader posee en exclusiva el I/O físico; el follower
  espera.
No existe un "sistema de rolling update" separado: es una operación de
mantenimiento del Subsistema A (y, con semántica distinta, de B vía
failover de leader).
### 3.2 Vista por capas
| Capa | Subsistema A (Réplica) | Subsistema B (Leader/Follower) | ¿Compartido? |
|---|---|---|---|
| **L1** ciclo de vida (señales / drain / probes) | igual | igual | **compartido** |
| **L3** traspaso sin downtime (socket activation) | cada réplica obtiene el fd de systemd | toma de fd leader→follower (avanzado) | **compartido** |
| **L2** coordinación | **2a** registro de pares + lock compartido (`pg_advisory`) | **2b** elección por lease + recurso exclusivo + registro leader/follower | **se bifurca** (mismo trait, política distinta) |
| **L4** orquestación | **4a** escalado de réplicas / rolling update | **4b** failover | **se bifurca** |
Conclusión clave: **L1 y L3 se comparten plenamente; L2/L4 se
bifurcan.** `CoordinationLock` es el mismo trait en 2a y 2b — usado para
coordinar escrituras concurrentes en A, y como lease del leader en B.
Esa unificación del trait es justamente el punto donde aterriza "los
principios son comunes".
## 4. Pertenencia de crates
El objetivo del usuario "ponlo en arona" debe partirse, porque **arona
hoy es un crate puramente de protocolo/tipos** — sólo dependencias
`serde` / `ts-rs` / `schemars`, `lib.rs:5` exige "cada tipo se define en
entelecheia y se consume en shittim-chest", y hace `exclude` de todos
los artefactos que no son de protocolo para publicar en crates.io.
Inyectar lógica de runtime (tokio, `sd_listen_fds`, manejo de señales)
rompería esa identidad ligera y publicable.
Partición:
- **`arona::lifecycle` (contrato de protocolo, vive en arona).** Sólo
  métodos y tipos JSON-RPC: `DrainState`, `ReadyStatus`,
  `Lifecycle.Drain`, `Lifecycle.Status`, `Worker.Status`, etc. Satisface
  la regla de arona de "emparejado en ambos lados".
- **`malkuth` (crate nuevo, runtime).** Depende de los tipos de
  protocolo de `arona` + `tokio` + un binding de `libsystemd` (socket
  activation) + traits de backend. Activado por features:
  - `replica` — coordinación y orquestación del Subsistema A.
  - `leader-follower` — elección por lease y failover del Subsistema B.
  - `socket-activation` — adquisición de fd de systemd.
  - `file-lock` / `pg-lock` / `lease` — backends de `CoordinationLock`.
Los tres proyectos dependen de `malkuth` y activan las features
que necesitan (véase la matriz del §8). Meterlo todo en arona la
forzaría a convertirse en "protocolo + runtime opcional" y destruiría su
pureza — no recomendado.
## 5. Abstracciones núcleo
### 5.1 `Worker` — un recurso de subproceso supervisado
Un `Worker` es un proceso independientemente-matable que posee
exactamente un recurso (una conexión a PLC, un puerto serie, un puerto
de escucha local, un sidecar como cosmos / pglite-proxy). El proceso es
el **límite de aislamiento de fallos**: un bug en el stack Modbus no
puede envenenar al worker S7comm.
Máquina de estados del ciclo de vida (tomada de
`evernight/src/model_server.rs:128-139`):
```
        inicio                     salud ok
 Starting ──────► Running ─────────────────► Running
     │              │  ▲
     │              │  │ salud ok (self-heal)
     │              ▼  │
     └──────► Failed ◄┘        fallo / no saludable
                  │
                  │ política de reinicio = permanent
                  └────────► Starting (rate-limitado)
```
### 5.2 `Supervisor` — posee el pool de workers
- **Política de reinicio** (vocabulario OTP): `permanent` (reiniciar
  siempre — por defecto para workers de recursos), `transient`
  (reiniciar sólo en salida anormal), `temporary` (nunca reiniciar).
- **Limitación de tasa con ventana deslizante** (tomada del
  `health_daemon` de entelecheia `max_restart_attempts` + `cooldown`):
  si un worker se reinicia más de N veces en la ventana W, entra en
  `cooldown` para prevenir tormentas de fallos; los reinicios posteriores
  se difieren.
### 5.3 `Lifecycle` — semántica uniforme de señales (Capa 1)
Adoptar la convención nginx/Go:
| Señal | Semántica | Comportamiento |
|---|---|---|
| `SIGINT` (ctrl_c) | equivalente a SIGTERM (cómodo para desarrollo) | entrar en drain |
| `SIGTERM` | **graceful shutdown** | drain: limpiar bit ready → dejar de aceptar → drenar lo en vuelo → salir |
| `SIGHUP` | **hot config reload** | no salir; releer configuración |
| `SIGQUIT` | **salida inmediata** (sólo emergencia) | saltar drain, salida rápida |
**Secuencia de drain** (una implementación; cada proyecto inyecta su
propio "closure de drain"):
1. Poner `/readyz` `draining = true` (el LB / orquestador lo ve y deja de
   enviar tráfico nuevo).
2. Detener el `accept` de nuevas conexiones (bajo socket activation:
   dejar de aceptar del fd heredado).
3. Enviar close frames a los WebSockets vivos; esperar las peticiones en
   vuelo con un timeout `DRAIN_TIMEOUT` (por defecto 30s, configurable).
4. Hacer drain de las tareas en background (copiar `TaskManager.stop_all`
   + `wait_all` de entelecheia).
5. Desconectar limpiamente los pools de upstream (copiar la desconexión
   limpia de `upstream_pool` de shittim-chest).
6. Liberar locks, limpiar archivos temporales → salir 0.
Nota de implementación: el `axum::serve(listener,
app).with_graceful_shutdown(...)` de axum ya soporta drain; **la pieza
clave que falta es cablear `SIGTERM`** (hoy sólo está cableado `ctrl_c`).
Referencias: `entelecheia/.../shutdown.rs:17`,
`shittim-chest/.../api.rs:465`, `evernight/src/api/mod.rs:109`.
### 5.4 Endpoints de salud (uniformes)
Separar probes (hoy los tres proyectos son inconsistentes):
| Endpoint | Semántica | Decisión |
|---|---|---|
| `/healthz` (liveness) | proceso vivo | 200 si el proceso puede responder (criterio simple de reinicio) |
| `/readyz` (readiness) | **puede servir**, lleva el bit de drain | 200 si no está en drain Y las dependencias están (ping de DB / socket scepter / primer poll de estación); 503 mientras hace drain |
El bit `draining` de `/readyz` es la señal central de rolling update: el
orquestador enruta peticiones nuevas sólo a instancias cuyo `/readyz` es
200. El `GET /api/health` existente de shittim-chest (`routes.rs:27`) se
eleva a `/readyz` con un bit de drain.
### 5.5 `acquire_listener` — traspaso sin downtime de la Capa 3
`malkuth` expone `acquire_listener(addr) -> TcpListener`:
1. Probar `sd_listen_fds()` (validar `LISTEN_PID`) — systemd está
   reteniendo el fd.
2. Fallback a `TcpListener::bind(addr)` plano (dev, sin systemd).
El `serve(listener, ...)` de axum ya acepta un listener pre-enlazado, así
que la fontanería existe; hoy sólo falta el *origen* del fd. Tres
adaptadores de despliegue:
| Despliegue | Enfoque | Aplicable a |
|---|---|---|
| **systemd puro** | instancias plantilla `xxx.socket` + `xxx@.service` | scepter, evernight-gateway, el propio malkuth |
| **docker** (shittim-chest prod) | socket activation de systemd en el host, pasar el socket/fd ya enlazado al contenedor (`LISTEN_FDS` + `SocketUser`); o un master ligero dentro del contenedor que retiene el fd | shittim-chest prod |
| **dev** | fallback a `bind` plano + solape breve (aceptar unos cientos de ms de conexiones caídas), sin systemd | dev de los tres |
Rolling update bajo socket activation:
```
[disparador de upgrade] → iniciar instancia nueva (service@new con plantilla, hereda/readquiere fd)
                        → hacer poll al /readyz de la nueva instancia hasta 200
                        → SIGTERM a la instancia vieja (= drain)
                        → la instancia vieja hace drain y se auto-sale
                        → systemd retiene el fd durante todo el proceso → cero conexiones caídas
```
### 5.6 `CoordinationLock` — trait de Capa 2 con backends
Durante la ventana de rolling update, las instancias vieja y nueva
pueden leer/escribir recursos compartidos de forma concurrente. Las
transacciones de DB son inherentemente seguras; los archivos (JSONL de
evernight, configs) necesitan "notificar-antes-de-escribir + lock".
```rust
pub trait CoordinationLock: Send + Sync {
    async fn acquire(&self, key: &str, lease: Duration) -> Result<LockGuard>;
}
// Backends:
//   FileLock  — flock/fcntl,       para evernight (JSONL / config)
//   PgLock    — pg_advisory_lock,  para entelecheia / shittim-chest
//   LeaseLock — file lock + lease (auto-expira al caer)
```
**Registro de instancias** (usado sólo durante la ventana de upgrade;
un único registro en estado estable): una pequeña tabla/archivo
compartido que registra `{instance_id, role: Active | Draining,
started_at, generation}`. La instancia nueva escribe una fila `Active`
al arrancar; en el upgrade, la vieja se marca `Draining`. Esto reemplaza
al quórum Raft que quedó explícitamente fuera de alcance — porque el
estado estable es de instancia única, el registro sólo coordina el
drain, sin necesidad de consistencia fuerte (basta un archivo o fila de
DB).
## 6. Subsistema A — Réplica (balanceo de carga ⊃ rolling update)
**Forma.** N instancias pares idénticas ejecutándose en paralelo detrás
de un LB frontal, estado en Postgres compartido. **active-active**, los
pares son iguales, sin leader.
| Preocupación | Enfoque |
|---|---|
| Enrutado de peticiones | LB frontal (caddy / `SO_REUSEPORT` round-robin integrado); eliminación por `/readyz` |
| R/W de estado compartido | transacciones de DB + `pg_advisory_lock` para coordinar escrituras concurrentes (naturalmente seguro) |
| WebSocket / conexiones largas | **sticky session** (el LB fija por cookie/instance id) o **migración de sesión** (el cliente se reconecta a cualquier réplica tras la desconexión; el estado se recupera de la DB — `upstream_pool` de shittim-chest ya es una plantilla de reconexión) |
| Afinidad de sesión | tanto entelecheia como shittim-chest externalizan el estado a Postgres y lo recuperan en el arranque → **naturalmente amigable a réplicas**, su gran ventaja sobre evernight |
| Rolling update | suboperación de escalado de réplicas: añadir réplica nueva (versión nueva) → ready → hacer drain y eliminar la réplica vieja → repetir. `maxSurge/maxUnavailable` de K8s en miniatura |
**Por qué A es comparativamente fácil.** Como entelecheia y
shittim-chest externalizan el estado a Postgres (recuperación en
arranque, confirmado por la auditoría), las réplicas no necesitan
replicar estado entre ellas — basta un LB frontal más transacciones de
DB. La única parte difícil es la stickiness / migración de WebSocket.
## 7. Subsistema B — Leader/Follower (edge HA activo-pasivo)
**Forma.** Dos procesos evernight en el mismo dispositivo/pasarela, uno
leader y uno follower; el `evernight-server` de upstream ve **un único
`node_id`** (un dispositivo). **active-pasivo**, las instancias no son
iguales, el leader posee en exclusiva el I/O físico.
### 7.1 El "truco" que simplifica B: árbol de supervisión + let-it-crash
En vez de hacer tolerante a fallos cada recurso, **sólo el supervisor se
hace leader/follower**; los recursos son procesos worker independientes
que simplemente se reinician al fallar. En concreto (según la decisión
acordada):
```
supervisor  (HA leader / follower)        ← sólo esta capa hace elección por lease + failover
   ├─ worker: PLC-A (Modbus)              ← reinicio supervisado, instancia única
   ├─ worker: PLC-B (S7comm)              ← reinicio supervisado, instancia única
   ├─ worker: serie / CAN                 ← reinicio supervisado, instancia única
   └─ worker: listener de puerto local    ← reinicio supervisado, instancia única
```
Este es el modelo de árbol de supervisión OTP / "let it crash" (también
systemd `Restart=always`, Pod de K8s). Beneficios:
- **Separación de responsabilidades.** Los workers sólo "retienen un
  recurso y trabajan"; no cargan con lógica de tolerancia a fallos,
  elección o sincronización de estado — se mantienen máximamente
  simples.
- **Tolerancia a fallos concentrada.** Sólo el supervisor hace HA; la
  complejidad colapsa a un único sitio.
- **Aislamiento de fallos.** Un fallo en un recurso (p.ej. un protocolo
  de PLC) no puede afectar a los demás (procesos separados).
- **Ajuste a los protocolos.** evernight habla muchos protocolos
  industriales (Modbus/S7/CAN/serie); mapear cada uno a un worker da el
  máximo valor de aislamiento.
### 7.2 Ciclo de vida del worker en el failover — modelo de subproceso (punto de partida acordado)
Los workers son **subprocesos** del supervisor (`kill_on_drop`). La
muerte del leader → los workers quedan huérfanos/muertos → el follower
promovido **vuelve a spawnear todos los workers** (cada PLC se
reconecta).
- Modelo más simple; encaja con la intención de "el servidor es el
  responsable".
- Coste: en el failover del supervisor, todos los recursos caen
  brevemente y se reconectan.
- Avanzado (diferido): workers como daemons independientes bajo un init
  inferior, el supervisor sólo los dirige por IPC; el nuevo supervisor
  hace `attach` de los workers que sobreviven. Menos interrupción, pero
  los workers deben implementar "re-enlazar al supervisor actual" — más
  complejo. Documentado como opción futura.
### 7.3 Elección de leader + fencing
- **Elección por lease.** El leader retiene un file lock + lease (con
  TTL), renueva en cada heartbeat; el follower hace polling; ante timeout
  del heartbeat del leader, el follower toma el lease y se autopromueve.
- **Recurso físico exclusivo.** Las conexiones PLC/serie/CAN sólo puede
  retenerlas el leader (dos procesos haciendo poll del mismo PLC entran
  en conflicto) → el leader hace poll, el follower espera. Esta es la
  razón fundamental de que B sea active-pasivo y no active-active.
- **Sincronización de estado.** **Standby en frío** como punto de
  partida (el follower no replica; al promoverse se recupera del JSONL
  en disco). Standby en caliente (el follower sigue el JSONL del leader)
  es una opción avanzada.
- **Fencing contra split-brain.** TTL del lease + fencing: el follower
  sólo puede tomar el control después de que el lease haya expirado
  realmente; tras tomarlo, se impide físicamente al leader viejo seguir
  escribiendo (la exclusividad del I/O físico es un fence natural).
- **Identidad única de dispositivo.** Leader y follower comparten un
  `node_id`; sólo el leader actual emite `device.register`.
Análogos clásicos: keepalived/VRRP, DRBD+Pacemaker, MySQL
primary/replica, Redis Sentinel — como una simplificación dentro de la
máquina, a nivel de proceso. La teoría (elección por lease + fencing)
está bien fundada.
## 8. Matriz de adopción por proyecto
| Proyecto | rol del supervisor | workers | estrategia |
|---|---|---|---|
| entelecheia (scepter) | una de las réplicas | sidecar cosmos, contenedores de agent | **A Réplica** |
| shittim-chest (chest) | una de las réplicas | pglite-proxy (mock), channel intake | **A Réplica** |
| dispositivo evernight (`sensor-poll`) | **leader / follower** | un worker por protocolo (Modbus/S7/CAN/serie) | **B Leader/Follower** |
| evernight-server (central) | una de las réplicas | contenedores model_server | **A Réplica** |
Selección de features por proyecto (`malkuth`):
- entelecheia / shittim-chest / evernight-server: `replica` +
  `socket-activation` + `pg-lock`; abstracción de worker para sus
  sidecars.
- dispositivo evernight: `leader-follower` + `socket-activation` (toma
  de fd avanzada) + `file-lock` / `lease`; abstracción de worker para
  los procesos por protocolo.
## 9. Fases de despliegue
1. **Fase A — Capa 1 en los tres proyectos.** Semántica de señales +
   `/healthz` / `/readyz` + drain. Menor riesgo, mayor beneficio
   inmediato (arregla primero el hard-kill por SIGTERM).
2. **Fase B — protocolo `arona::lifecycle` + esqueleto de
   `malkuth`.** Definiciones de trait, `acquire_listener`, trait
   `CoordinationLock` + backends `FileLock` / `PgLock`, primitivas
   `Worker` + `Supervisor`.
3. **Fase C — Capa 3.** Unidades de socket-activation para los tres
   proyectos + el adaptador docker + fallback de dev.
4. **Fase D — Capa 4.** Orquestador de cola de manifiestos (copiar
   `health_daemon`) + bucle de drain de coexistencia viejo/nuevo,
   incluido el flujo de dev "compilar servidor nuevo"; más el failover
   leader/follower para B.
## 10. Riesgos y límites
- **shittim-chest docker + socket activation** es la pieza más incierta;
  el traspaso del fd al contenedor necesita un spike de prototipo. Si no
  es factible, recurrir a "caddy externo + eliminación por `/readyz` +
  solape breve".
- **El estado en memoria de evernight** (`DeviceRegistry`, sesiones) se
  pierde en el drain; decidir si persistir o migrar (en el peor caso: la
  nueva instancia reconstruye, las sesiones largas caen).
- **Drain de WebSocket ≠ migración.** El WS en vuelo aún cae cuando la
  instancia vieja sale; "sin costuras" exige que el cliente se
  reconecte a una instancia nueva (el cliente de shittim-chest ya tiene
  lógica de reconexión de `upstream_pool`, reutilizable).
- **Tolerancia a fallos a nivel de proceso vs. de hilo.** Leader/follower
  (Subsistema B) resuelve la tolerancia a fallos de *nivel
  proceso/instancia*, no de nivel hilo. Un panic de tokio dentro de una
  tarea es trabajo del supervisor (reiniciar tarea, no failover de
  proceso). No usar B para absorber caídas de hilos — es demasiado
  pesado.
- **Explícitamente fuera de alcance.** Quórum Raft, sharding por hash
  consistente, HA cross-datacenter — excluidos por el alcance de
  "rolling-update + edge HA". El estado estable de instancia única
  significa que el registro sólo coordina el drain.
## 11. Preguntas abiertas (diferidas)
- Worker en el failover: modelo de subproceso elegido como punto de
  partida; el modelo "daemon independiente + attach" queda documentado
  como avanzado.
- Standby en frío vs. caliente para B: elegido frío para el punto de
  partida (recuperar del JSONL al promover); caliente (el follower sigue
  el log del leader) diferido.
- Si también envolver el sidecar `cosmos` de entelecheia y el
  `pglite-proxy` de shittim-chest bajo la abstracción unificada de
  `Worker`: acordado que sí (unificar en una única abstracción de worker
  en los tres proyectos).
---
*Traducción: la fuente canónica en inglés es
`docs/en/design/platform/supervision-and-rolling-update.md`. Existe
además la versión en chino simplificado (`docs/zhs/...`). El resto de
idiomas (zht/ja/ko/fr/ru) están pendientes de i18n.*
