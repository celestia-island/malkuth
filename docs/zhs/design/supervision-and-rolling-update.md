# 统一监督、滚动更新与副本架构
> **范围。** 本文是*平台级*设计:它横切 `core`(entelecheia / scepter)、
> `webui`(shittim-chest / chest)与 `router`(evernight)。各项目自身的架构
> 文档位于其 `core/`、`webui/`、`router/` 子分类下;本文定义三者共同消费的
> 共享生命周期 / 监督层。
## 1. 背景与目标
三个项目在结构上高度同构——均为 **Rust(edition 2024,MSRV 1.85)+ axum
0.8 + tokio + 基于 Unix 套接字 / WebSocket 的 JSON-RPC**,且都已共享
`plana` crate 作为协议层。正是这种同构,使得*做一套*监督机制、三处复用
是值得的。
这套机制必须把四个相互重叠的需求作为同一个连贯设施来服务:
1. **负载均衡** —— 同一个程序同时启动多个实例分担任务,彼此通过 IPC
   协调,同时还能共享数据库 / 配置 / 运行时状态。
2. **协调写入** —— 一方准备写共享文件时,必须通知另一方并上锁,避免并发
   修改损坏状态。
3. **滚动更新** —— 收到新的官方发布(或本地动态编译出的新服务端)时,
   新旧二进制可以并存;旧进程处理完存量任务后退出,把运行集交给新进程。
4. **边缘容错** —— 在设备上(尤其是 evernight 网关)两个进程以主/备形式
   运行,其中一个崩溃不至于让整台设备宕机。
### 1.1 现状(本设计要补的缺口)
代码审计发现**三个项目**存在同样的三类缺陷:
| 能力 | entelecheia(scepter) | shittim-chest(chest) | evernight |
|---|---|---|---|
| 信号处理 | 仅 `ctrl_c`(`shutdown.rs:17`) | 仅 `ctrl_c`(`api.rs:465`) | 仅 `ctrl_c`(`api/mod.rs:109`) |
| 排空逻辑 | 仅排空 HTTP,不排空 WS / 后台任务 | 同左 | 无 |
| 监听 fd 传递 | 无 | 无 | 无 |
| 带 drain 位的 `/readyz` | 无 | 有 `/api/health`,无 drain 位 | 无 |
最致命的问题:因为只捕获了 `SIGINT`,而 `docker stop` / `systemctl
restart` 发送的是 **`SIGTERM`**,优雅关闭路径被完全绕过,宽限期后直接被
硬杀。光修这一点,就是收益最高的单点改动。
### 1.2 可复用的现有资产
- `entelecheia/packages/cli/src/evernight_daemon.rs` —— 当前最完整的自重启
  蓝图:PID 锁文件 + 自我 reexec + 就绪等待 + `SIGTERM`→`SIGKILL` 兜底。
- `entelecheia/packages/scepter/src/daemon/health_daemon.rs` —— 用 **JSON
  manifest 文件队列**做容器滚动更新(一种语言无关的更新原语)。
- `entelecheia/packages/shared/infra_jsonrpc` —— Unix 套接字 JSON-RPC 传输层。
- `shittim-chest/packages/core/src/proxy/upstream_pool.rs` —— 多端点注册表 +
  指数退避重连;负载均衡客户端的现成模板。
- `evernight/src/model_server.rs` —— `Running/Starting/Stopped/Failed` 资源
  生命周期状态机,含"部署→等健康→停旧";仓库内应用层滚动更新的模板。
## 2. 理论基础
这套机制不是单一理论,而是若干成熟工业模式的组合,每一种都有标杆实现。
此处显式命名,以便设计直接继承它们已被验证的语义:
| 表达的需求 | 业界术语 | 标杆实现 |
|---|---|---|
| 新旧进程并存;旧的把存量干完再退 | **优雅停机 / 排空(drain)+ 滚动更新** | Kubernetes Deployment、nginx / unicorn |
| "红蓝标记"(讨论中回忆的名字) | **蓝绿部署**(两套并行环境,切流量指针)。所描述的渐进消亡更像**滚动更新 + drain**,而非蓝绿。 | |
| 新进程接管同一端口、不丢连接 | **socket activation / fd 继承 / `SO_REUSEPORT`** | systemd、nginx `USR2`、envoy hot restart |
| "写之前通知对方并上锁" | **劝告锁(`flock`/`fcntl`)/ DB 行锁 / 租约(lease)** | POSIX 劝告锁、`pg_advisory_lock` |
| 进程自愈 | **监督树(OTP)/ systemd / kubelet** | Erlang/OTP、systemd、s6、immortal |
| 主/备,使崩溃不至于杀掉整个服务 | **基于租约的选主 + fencing** | Chubby 租约、Raft 选主(子集)、keepalived/VRRP、Pacemaker |
| 资源由一个进程持有,挂了就重启 | **"let it crash" + 监督重启** | Erlang/OTP 监督树、systemd `Restart=always` |
**滚动更新的工业标准配方**(值得照抄)就是 Kubernetes 的:
`maxSurge + maxUnavailable + readinessProbe + preStop hook + SIGTERM
优雅关闭 + 宽限期 + PodDisruptionBudget`。我们做的是它的自托管、单机 /
小集群变体。
**nginx 热升级**是"新旧并存,然后旧的排空"的教科书:`USR2` 启动新
master 继承 listen fd → `WINCH` 优雅停旧 worker → `QUIT` 退役旧 master。
§7 的流程与之同构。
## 3. 总体架构
设计收敛到一个优雅的统一骨架:**到处都是监督树;唯一的区别是 supervisor
究竟是对等副本,还是主/备。**
```
                    ┌─────────────────────────────────────────┐
   共享基座          │ 信号语义 · 排空(drain)                  │  ← 三个项目
   (Layer 1 / 3)    │ /healthz · /readyz(含 drain 位)        │     完全共用
                    │ socket activation + 3 种部署适配         │
                    └─────────────────────────────────────────┘
                                   │
          ┌────────────────────────┴────────────────────────┐
          ▼                                                  ▼
   ┌────────────────┐                              ┌──────────────────┐
   │ 子系统 A       │  supervisor 对等             │ 子系统 B         │  supervisor 是
   │ 副本(Replica) │  (active-active)             │ 主备             │  leader 或 follower
   │                │  ← 服务端负载均衡 + 滚动更新 │ (Leader/Follower)│  ← 边缘容错
   └───────┬────────┘                              └─────────┬────────┘
           │                                                  │
           └──────────────────┬───────────────────────────────┘
                              ▼
               ┌──────────────────────────────┐
               │ 统一 Worker 抽象             │  ← 三个项目所有"子进程资源"
               │ 生命周期状态机 + 监督重启    │     cosmos / pglite-proxy / 各协议 worker
               │ (permanent/transient)+ 限频  │
               └──────────────────────────────┘
```
### 3.1 关键简化:滚动更新*不是*第三套系统
一旦监督树成为骨架,用户的需求就坍缩为**两**个子系统,而非三套:
- **子系统 A —— 副本(Replica)。** 服务端跑 N 个相同的对等实例分担负载。
  *负载均衡和滚动更新是同一套*:滚动更新就是"副本数临时 +1 → 排空掉一个
  旧副本 → 重复"。它就是 Kubernetes 的 `maxSurge/maxUnavailable` 操作,
  以副本增减来表达。
- **子系统 B —— 主备(Leader/Follower)。** 边缘(evernight 设备)跑两个
  进程,一主一备,用于容错。主独占物理 I/O;备待命。
没有单独的"滚动更新系统":它是子系统 A 的一个维护操作(以及,语义不同地,
通过主切换作用于 B)。
### 3.2 分层视图
| 层 | 子系统 A(副本) | 子系统 B(主备) | 共用? |
|---|---|---|---|
| **L1** 生命周期(信号 / 排空 / 探针) | 相同 | 相同 | **共用** |
| **L3** 零停机交接(socket activation) | 每副本从 systemd 拿 fd | 主→备 fd 接管(进阶) | **共用** |
| **L2** 协调 | **2a** 对等注册表 + 共享锁(`pg_advisory`) | **2b** 租约选主 + 排他资源 + 主备注册 | **分叉**(同一 trait,不同策略) |
| **L4** 编排 | **4a** 副本伸缩 / 滚动更新 | **4b** 故障转移 | **分叉** |
洞察:**L1 与 L3 完全共用;L2/L4 分叉。** `CoordinationLock` 在 2a 与 2b
是同一个 trait——在 A 里用于协调并发写,在 B 里用作主租约。这个 trait 统一,
正是"原理相通"的落点。
## 4. crate 归属
用户目标"放进 plana"必须拆分,因为 **plana 当前是纯协议/类型 crate**——
只有 `serde` / `ts-rs` / `schemars` 依赖,`lib.rs:5` 要求"每个类型在
entelecheia 定义、被 shittim-chest 消费",并为发布 crates.io 而
`exclude` 了所有非协议产物。注入运行时逻辑(tokio、`sd_listen_fds`、信号
处理)会破坏它轻量、可发布的定位。
拆分:
- **`plana::lifecycle`(协议契约,放进 plana)。** 只放 JSON-RPC 方法与类型:
  `DrainState`、`ReadyStatus`、`Lifecycle.Drain`、`Lifecycle.Status`、
  `Worker.Status` 等。符合 plana"双方配对"的规则。
- **`malkuth`(新 crate,运行时)。** 依赖 `plana` 协议类型 +
  `tokio` + `libsystemd` 绑定(socket activation)+ 后端 trait。按 feature
  开启:
  - `replica` —— 子系统 A 的协调与编排。
  - `leader-follower` —— 子系统 B 的租约选主与故障转移。
  - `socket-activation` —— systemd fd 获取。
  - `file-lock` / `pg-lock` / `lease` —— `CoordinationLock` 后端。
三个项目依赖 `malkuth`,按需开启 feature(见 §8 矩阵)。把一切塞进
plana 会迫使它变成"协议 + 可选运行时",毁掉其纯净性——不推荐。
## 5. 核心抽象
### 5.1 `Worker` —— 一个被监督的子进程资源
一个 `Worker` 是一个可独立杀死的进程,它恰好持有一个资源(一条 PLC 连接、
一个串口、一个本地监听端口、一个如 cosmos / pglite-proxy 的 sidecar)。进程
即**故障隔离边界**:Modbus 栈的 bug 不会污染 S7comm worker。
生命周期状态机(取自 `evernight/src/model_server.rs:128-139`):
```
        启动                       健康检查通过
 Starting ──────► Running ─────────────────► Running
     │              │  ▲
     │              │  │ 健康检查通过(自愈)
     │              ▼  │
     └──────► Failed ◄┘        崩溃 / 不健康
                  │
                  │ 重启策略 = permanent
                  └────────► Starting(受限频控制)
```
### 5.2 `Supervisor` —— 持有 worker 池
- **重启策略**(OTP 词汇):`permanent`(总重启——资源 worker 默认)、
  `transient`(仅异常退出才重启)、`temporary`(从不重启)。
- **滑动窗口限频**(取自 entelecheia `health_daemon` 的
  `max_restart_attempts` + `cooldown`):若某 worker 在窗口 W 内重启超过
  N 次,进入 `cooldown`,以防崩溃风暴;后续重启被推迟。
### 5.3 `Lifecycle` —— 统一信号语义(Layer 1)
采用 nginx/Go 约定:
| 信号 | 语义 | 行为 |
|---|---|---|
| `SIGINT`(ctrl_c) | 等价 SIGTERM(对开发友好) | 进入排空 |
| `SIGTERM` | **优雅停机** | 排空:清 ready 位 → 停止 accept → 排空在途 → 退出 |
| `SIGHUP` | **热重载配置** | 不退出;重读配置 |
| `SIGQUIT` | **立即退出**(仅紧急) | 跳过排空,快速退出 |
**排空序列**(一份实现;各项目注入各自的"排空闭包"):
1. 置 `/readyz` 的 `draining = true`(LB / 编排器据此停止派发新流量)。
2. 停止 `accept` 新连接(socket activation 下:停止从继承的 fd accept)。
3. 给存活 WebSocket 发 close 帧;带超时 `DRAIN_TIMEOUT`(默认 30s,可配)
   等待在途请求。
4. 排空后台任务(抄 entelecheia `TaskManager.stop_all` + `wait_all`)。
5. 干净断开上游连接池(抄 shittim-chest `upstream_pool` 的干净断开)。
6. 释放锁、清理临时文件 → 退出 0。
实现要点:axum 的 `axum::serve(listener,
app).with_graceful_shutdown(...)` 已支持排空;**关键是把 `SIGTERM` 接进去**
(今天只接了 `ctrl_c`)。参考:`entelecheia/.../shutdown.rs:17`、
`shittim-chest/.../api.rs:465`、`evernight/src/api/mod.rs:109`。
### 5.4 健康端点(统一)
探针分离(今天三项目不一致):
| 端点 | 语义 | 判定 |
|---|---|---|
| `/healthz`(liveness) | 进程活着 | 进程能响应即 200(简单的重启判据) |
| `/readyz`(readiness) | **能接活**,带 drain 位 | 未排空且依赖就绪(DB ping / scepter 套接字 / 站点首轮轮询)→ 200;排空中 → 503 |
`/readyz` 的 `draining` 位是滚动更新的核心信号:编排器只把新请求路由到
`/readyz` 为 200 的实例。shittim-chest 现有 `GET /api/health`
(`routes.rs:27`)升级为带 drain 位的 `/readyz` 即可。
### 5.5 `acquire_listener` —— Layer 3 零停机交接
`malkuth` 暴露 `acquire_listener(addr) -> TcpListener`:
1. 优先 `sd_listen_fds()`(校验 `LISTEN_PID`)—— systemd 持有 fd。
2. 回退到普通 `TcpListener::bind(addr)`(dev、无 systemd)。
axum `serve(listener, ...)` 本就接受预绑定 listener,管道齐备;今天只差 fd
的*来源*。三种部署适配:
| 部署 | 方案 | 适用 |
|---|---|---|
| **裸 systemd** | `xxx.socket` + `xxx@.service` 模板实例化 | scepter、evernight-gateway、malkuth 自身 |
| **docker**(shittim-chest 生产) | 宿主机 systemd socket activation,把已绑定的套接字/fd 透传进容器(`LISTEN_FDS` + `SocketUser`);或容器内跑一个轻量 master 持 fd | shittim-chest 生产 |
| **dev** | 回退普通 `bind` + 短暂重叠(接受几百 ms 丢连接),无 systemd | 三项目 dev |
socket activation 下的滚动更新:
```
[升级触发] → 启动新实例(模板化 service@new,继承/重拿 fd)
            → 轮询新实例 /readyz 直到 200
            → 给旧实例发 SIGTERM(= 排空)
            → 旧实例排空后自退
            → systemd 全程持有 fd → 零丢连接
```
### 5.6 `CoordinationLock` —— Layer 2 trait 与后端
滚动更新窗口里,新旧实例可能并发读写共享资源。DB 事务天然安全;文件
(evernight JSONL、配置)需要"写前通知 + 上锁"。
```rust
pub trait CoordinationLock: Send + Sync {
    async fn acquire(&self, key: &str, lease: Duration) -> Result<LockGuard>;
}
// 后端:
//   FileLock  — flock/fcntl,        适配 evernight(JSONL / 配置)
//   PgLock    — pg_advisory_lock,   适配 entelecheia / shittim-chest
//   LeaseLock — 文件锁 + 租约(崩溃自动过期)
```
**实例注册表**(仅在升级窗口使用;常态单条记录):一个小的共享表/文件,记录
`{instance_id, role: Active | Draining, started_at, generation}`。新实例
启动写一条 `Active`;升级时把旧的标 `Draining`。它替代了被明确排除在外的
Raft 仲裁——因为常态单实例,注册表只用于协调排空,无强一致需求(一个文件
或 DB 行即可)。
## 6. 子系统 A —— 副本(负载均衡 ⊃ 滚动更新)
**形态。** N 个相同对等实例在前置 LB 后并行运行,状态在共享 Postgres。
**active-active**,实例对等,无主。
| 关切点 | 方案 |
|---|---|
| 请求路由 | 前置 LB(caddy / 内置 `SO_REUSEPORT` 轮询);按 `/readyz` 摘除 |
| 共享状态读写 | DB 事务 + `pg_advisory_lock` 协调并发写(天然安全) |
| WebSocket / 长连接 | **粘性会话**(LB 按 cookie/实例 id 固定)或**会话迁移**(断开后客户端重连到任一副本,状态从 DB 恢复——shittim-chest `upstream_pool` 已是重连模板) |
| 会话亲和 | entelecheia 与 shittim-chest 都把状态外置到 Postgres 且启动即恢复 → **天然副本友好**,这是它们相对 evernight 的巨大优势 |
| 滚动更新 | 副本伸缩的子操作:加新副本(新版本)→ ready → 排空并移除旧副本 → 重复。即 K8s `maxSurge/maxUnavailable` 的微缩版 |
**为什么 A 相对简单。** 因为 entelecheia 与 shittim-chest 把状态外置到
Postgres(启动恢复,审计已确认),副本间无需复制状态——前置 LB 加 DB 事务
就够。唯一难点是 WebSocket 的粘性 / 迁移。
## 7. 子系统 B —— 主备(边缘 active-passive HA)
**形态。** 同一台设备/网关上两个 evernight 进程,一主一备;上游
`evernight-server` 看到的是**同一个 `node_id`**(一个设备)。
**active-passive**,实例不对等,主独占物理 I/O。
### 7.1 简化 B 的"取巧":监督树 + let-it-crash
不要给每个资源都做容错,**只让 supervisor 做主备**;资源是独立的 worker 进
程,挂了直接重启。具体(按已定决策):
```
supervisor  (主 / 备 HA)              ← 只有这层做租约选主 + 故障转移
   ├─ worker: PLC-A(Modbus)           ← 监督重启,单实例
   ├─ worker: PLC-B(S7comm)           ← 监督重启,单实例
   ├─ worker: 串口 / CAN               ← 监督重启,单实例
   └─ worker: 本地端口监听             ← 监督重启,单实例
```
这就是 OTP 监督树 / "let it crash" 模型(也是 systemd `Restart=always`、
K8s Pod)。收益:
- **关注点分离。** worker 只"持有一个资源、干活",不背容错、选主、状态同步
  逻辑——保持极简。
- **容错集中。** 只有 supervisor 做 HA;复杂度收敛到一处。
- **故障隔离。** 某个资源(如某 PLC 协议)崩溃不影响其他(独立进程)。
- **契合协议多样性。** evernight 说多种工业协议(Modbus/S7/CAN/串口);每个
  对应一个 worker,隔离价值最大。
### 7.2 故障转移时 worker 的处理 —— 子进程模型(已定起步方案)
worker 是 supervisor 的**子进程**(`kill_on_drop`)。主挂 → worker 成孤儿/
被杀 → 提升后的备**重新拉起全部 worker**(每条 PLC 重连)。
- 最简单的模型;契合"服务端负责"的意图。
- 代价:supervisor 故障转移时,所有资源短暂中断重连。
- 进阶(延后):worker 作为更底层 init 下的独立常驻进程,supervisor 只通过
  IPC 指挥;新 supervisor `attach` 存活的 worker。中断更小,但 worker 需实现
  "重新认主"——更复杂。作为进阶选项记录在案。
### 7.3 选主 + fencing
- **租约选主。** 主持有文件锁 + 租约(带 TTL),每次心跳续租;备轮询;主心跳
  超时 → 备夺租约 → 自我提升。
- **排他物理资源。** PLC/串口/CAN 连接只能由主持有(两进程同时 poll 同一
  PLC 会冲突)→ 主 poll,备待命。这是 B 必须 active-passive 而非
  active-active 的根本原因。
- **状态同步。** 起步**冷备**(备不复制;提升时从盘上 JSONL 恢复)。热备
  (备追主的 JSONL 日志)为进阶选项。
- **脑裂 fencing。** 租约 TTL + fencing:备必须在租约真正过期后才能夺主;夺
  主后,旧主被物理阻止再写(物理 I/O 独占性是天然 fence)。
- **单一设备身份。** 主备共用一个 `node_id`;只有当前主发 `device.register`。
经典对照:keepalived/VRRP、DRBD+Pacemaker、MySQL 主从、Redis Sentinel——
作为单机内、进程级的简化版。理论(租约选主 + fencing)基础扎实。
## 8. 各项目适配矩阵
| 项目 | supervisor 角色 | worker 们 | 策略 |
|---|---|---|---|
| entelecheia(scepter) | 副本之一 | cosmos sidecar、agent 容器 | **A 副本** |
| shittim-chest(chest) | 副本之一 | pglite-proxy(mock)、channel intake | **A 副本** |
| evernight 设备(`sensor-poll`) | **主 / 备** | 每协议一个 worker(Modbus/S7/CAN/串口) | **B 主备** |
| evernight-server(中心) | 副本之一 | model_server 容器 | **A 副本** |
各项目的 feature 选择(`malkuth`):
- entelecheia / shittim-chest / evernight-server:`replica` +
  `socket-activation` + `pg-lock`;其 sidecar 走 worker 抽象。
- evernight 设备:`leader-follower` + `socket-activation`(进阶 fd 接管)+
  `file-lock` / `lease`;每协议进程走 worker 抽象。
## 9. 落地阶段
1. **阶段 A —— Layer 1 跨三项目落地。** 信号语义 + `/healthz` / `/readyz`
   + 排空。风险最低,立竿见影(先把 SIGTERM 硬杀修掉)。
2. **阶段 B —— `plana::lifecycle` 协议 + `malkuth` 骨架。** trait
   定义、`acquire_listener`、`CoordinationLock` trait + `FileLock` /
   `PgLock` 后端、`Worker` + `Supervisor` 原语。
3. **阶段 C —— Layer 3。** 三项目的 socket activation 单元 + docker 适配 +
   dev 回退。
4. **阶段 D —— Layer 4。** manifest 队列编排器(抄 `health_daemon`)+ 新旧
   并存排空闭环,含 dev"编译新服务端"流程;以及 B 的主备故障转移。
## 10. 风险与边界
- **shittim-chest docker + socket activation** 是最不确定的一环;fd 透传进
  容器需做原型验证。若不可行,退化为"外部 caddy + `/readyz` 摘除 + 短暂
  重叠"。
- **evernight 内存态**(`DeviceRegistry`、会话)排空时会丢;需评估是否落盘
  或迁移(最坏情况:新实例重建,长会话断)。
- **WebSocket 排空 ≠ 迁移。** 旧实例退出时,在途 WS 仍会断;"无缝"要求客户
  端重连到新实例(shittim-chest 客户端已有 `upstream_pool` 重连逻辑,可复用)。
- **进程级 vs 线程级容错。** 主备(子系统 B)解决的是*进程/实例级*容错,不是
  线程级。tokio 任务内 panic 是 supervisor 的活(任务重启,而非进程故障转
  移)。不要用 B 去扛线程崩溃——那太重了。
- **明确不在范围。** Raft 仲裁、一致哈希分片、跨机房 HA——已被"滚动更新 +
  边缘 HA"的范围排除。常态单实例意味着注册表只用于协调排空。
## 11. 待定问题(延后)
- 故障转移时的 worker:起步选子进程模型;"独立常驻 + attach"模型作为进阶记
  录在案。
- B 的冷备 vs 热备:起步选冷备(提升时从 JSONL 恢复);热备(备追主日志)延
  后。
- 是否把 entelecheia 的 `cosmos` sidecar 与 shittim-chest 的
  `pglite-proxy` 也纳入统一 `Worker` 抽象:已同意(三项目统一进同一套
  worker 抽象)。
---
*对应英文权威源:`docs/en/design/platform/supervision-and-rolling-update.md`。
其他语言(zht/ja/ko/fr/es/ru)的翻译待 i18n。*
