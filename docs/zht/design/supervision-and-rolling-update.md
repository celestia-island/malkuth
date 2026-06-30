# 統一監督、滾動更新與副本架構
> **範圍。** 本文是*平臺級*設計:它橫切 `core`(entelecheia / scepter)、
> `webui`(shittim-chest / chest)與 `router`(evernight)。各專案自身的架構
> 文件位於其 `core/`、`webui/`、`router/` 子分類下;本文定義三者共同消費的
> 共享生命週期 / 監督層。
## 1. 背景與目標
三個專案在結構上高度同構——均為 **Rust(edition 2024,MSRV 1.85)+ axum
0.8 + tokio + 基於 Unix 套接字 / WebSocket 的 JSON-RPC**,且都已共享
`arona` crate 作為協議層。正是這種同構,使得*做一套*監督機制、三處複用
是值得的。
這套機制必須把四個相互重疊的需求作為同一個連貫設施來服務:
1. **負載平衡** —— 同一個程式同時啟動多個例項分擔任務,彼此透過 IPC
   協調,同時還能共享資料庫 / 配置 / 執行時狀態。
2. **協調寫入** —— 一方準備寫共享檔案時,必須通知另一方並上鎖,避免併發
   修改損壞狀態。
3. **滾動更新** —— 收到新的官方釋出(或本地動態編譯出的新服務端)時,
   新舊二進位制可以並存;舊程序處理完存量任務後退出,把執行集交給新程序。
4. **邊緣容錯** —— 在裝置上(尤其是 evernight 閘道器)兩個程序以主/備形式
   執行,其中一個崩潰不至於讓整臺裝置宕機。
### 1.1 現狀(本設計要補的缺口)
程式碼審計發現**三個專案**存在同樣的三類缺陷:
| 能力 | entelecheia(scepter) | shittim-chest(chest) | evernight |
|---|---|---|---|
| 訊號處理 | 僅 `ctrl_c`(`shutdown.rs:17`) | 僅 `ctrl_c`(`api.rs:465`) | 僅 `ctrl_c`(`api/mod.rs:109`) |
| 排空邏輯 | 僅排空 HTTP,不排空 WS / 後臺任務 | 同左 | 無 |
| 監聽 fd 傳遞 | 無 | 無 | 無 |
| 帶 drain 位的 `/readyz` | 無 | 有 `/api/health`,無 drain 位 | 無 |
最致命的問題:因為只捕獲了 `SIGINT`,而 `docker stop` / `systemctl
restart` 傳送的是 **`SIGTERM`**,優雅關閉路徑被完全繞過,寬限期後直接被
硬殺。光修這一點,就是收益最高的單點改動。
### 1.2 可複用的現有資產
- `entelecheia/packages/cli/src/evernight_daemon.rs` —— 當前最完整的自重啟
  藍圖:PID 鎖檔案 + 自我 reexec + 就緒等待 + `SIGTERM`→`SIGKILL` 兜底。
- `entelecheia/packages/scepter/src/daemon/health_daemon.rs` —— 用 **JSON
  manifest 檔案佇列**做容器滾動更新(一種語言無關的更新原語)。
- `entelecheia/packages/shared/infra_jsonrpc` —— Unix 套接字 JSON-RPC 傳輸層。
- `shittim-chest/packages/core/src/proxy/upstream_pool.rs` —— 多端點登錄檔 +
  指數退避重連;負載平衡客戶端的現成模板。
- `evernight/src/model_server.rs` —— `Running/Starting/Stopped/Failed` 資源
  生命週期狀態機,含"部署→等健康→停舊";倉庫內應用層滾動更新的模板。
## 2. 理論基礎
這套機制不是單一理論,而是若干成熟工業模式的組合,每一種都有標杆實現。
此處顯式命名,以便設計直接繼承它們已被驗證的語義:
| 表達的需求 | 業界術語 | 標杆實現 |
|---|---|---|
| 新舊程序並存;舊的把存量幹完再退 | **優雅停機 / 排空(drain)+ 滾動更新** | Kubernetes Deployment、nginx / unicorn |
| "紅藍標記"(討論中回憶的名字) | **藍綠部署**(兩套並行環境,切流量指標)。所描述的漸進消亡更像**滾動更新 + drain**,而非藍綠。 | |
| 新程序接管同一埠、不丟連線 | **socket activation / fd 繼承 / `SO_REUSEPORT`** | systemd、nginx `USR2`、envoy hot restart |
| "寫之前通知對方並上鎖" | **勸告鎖(`flock`/`fcntl`)/ DB 行鎖 / 租約(lease)** | POSIX 勸告鎖、`pg_advisory_lock` |
| 程序自愈 | **監督樹(OTP)/ systemd / kubelet** | Erlang/OTP、systemd、s6、immortal |
| 主/備,使崩潰不至於殺掉整個服務 | **基於租約的選主 + fencing** | Chubby 租約、Raft 選主(子集)、keepalived/VRRP、Pacemaker |
| 資源由一個程序持有,掛了就重啟 | **"let it crash" + 監督重啟** | Erlang/OTP 監督樹、systemd `Restart=always` |
**滾動更新的工業標準配方**(值得照抄)就是 Kubernetes 的:
`maxSurge + maxUnavailable + readinessProbe + preStop hook + SIGTERM
優雅關閉 + 寬限期 + PodDisruptionBudget`。我們做的是它的自託管、單機 /
小叢集變體。
**nginx 熱升級**是"新舊並存,然後舊的排空"的教科書:`USR2` 啟動新
master 繼承 listen fd → `WINCH` 優雅停舊 worker → `QUIT` 退役舊 master。
§7 的流程與之同構。
## 3. 總體架構
設計收斂到一個優雅的統一骨架:**到處都是監督樹;唯一的區別是 supervisor
究竟是對等副本,還是主/備。**
```
                    ┌─────────────────────────────────────────┐
   共享基座          │ 訊號語義 · 排空(drain)                  │  ← 三個專案
   (Layer 1 / 3)    │ /healthz · /readyz(含 drain 位)        │     完全共用
                    │ socket activation + 3 種部署適配         │
                    └─────────────────────────────────────────┘
                                   │
          ┌────────────────────────┴────────────────────────┐
          ▼                                                  ▼
   ┌────────────────┐                              ┌──────────────────┐
   │ 子系統 A       │  supervisor 對等             │ 子系統 B         │  supervisor 是
   │ 副本(Replica) │  (active-active)             │ 主備             │  leader 或 follower
   │                │  ← 服務端負載平衡 + 滾動更新 │ (Leader/Follower)│  ← 邊緣容錯
   └───────┬────────┘                              └─────────┬────────┘
           │                                                  │
           └──────────────────┬───────────────────────────────┘
                              ▼
               ┌──────────────────────────────┐
               │ 統一 Worker 抽象             │  ← 三個專案所有"子程序資源"
               │ 生命週期狀態機 + 監督重啟    │     cosmos / pglite-proxy / 各協議 worker
               │ (permanent/transient)+ 限頻  │
               └──────────────────────────────┘
```
### 3.1 關鍵簡化:滾動更新*不是*第三套系統
一旦監督樹成為骨架,使用者的需求就坍縮為**兩**個子系統,而非三套:
- **子系統 A —— 副本(Replica)。** 服務端跑 N 個相同的對等例項分擔負載。
  *負載平衡和滾動更新是同一套*:滾動更新就是"副本數臨時 +1 → 排空掉一個
  舊副本 → 重複"。它就是 Kubernetes 的 `maxSurge/maxUnavailable` 操作,
  以副本增減來表達。
- **子系統 B —— 主備(Leader/Follower)。** 邊緣(evernight 裝置)跑兩個
  程序,一主一備,用於容錯。主獨佔物理 I/O;備待命。
沒有單獨的"滾動更新系統":它是子系統 A 的一個維護操作(以及,語義不同地,
透過主切換作用於 B)。
### 3.2 分層檢視
| 層 | 子系統 A(副本) | 子系統 B(主備) | 共用? |
|---|---|---|---|
| **L1** 生命週期(訊號 / 排空 / 探針) | 相同 | 相同 | **共用** |
| **L3** 零停機交接(socket activation) | 每副本從 systemd 拿 fd | 主→備 fd 接管(進階) | **共用** |
| **L2** 協調 | **2a** 對等登錄檔 + 共享鎖(`pg_advisory`) | **2b** 租約選主 + 排他資源 + 主備註冊 | **分叉**(同一 trait,不同策略) |
| **L4** 編排 | **4a** 副本伸縮 / 滾動更新 | **4b** 故障轉移 | **分叉** |
洞察:**L1 與 L3 完全共用;L2/L4 分叉。** `CoordinationLock` 在 2a 與 2b
是同一個 trait——在 A 裡用於協調併發寫,在 B 裡用作主租約。這個 trait 統一,
正是"原理相通"的落點。
## 4. crate 歸屬
使用者目標"放進 arona"必須拆分,因為 **arona 當前是純協議/型別 crate**——
只有 `serde` / `ts-rs` / `schemars` 依賴,`lib.rs:5` 要求"每個型別在
entelecheia 定義、被 shittim-chest 消費",併為釋出 crates.io 而
`exclude` 了所有非協議產物。注入執行時邏輯(tokio、`sd_listen_fds`、訊號
處理)會破壞它輕量、可釋出的定位。
拆分:
- **`arona::lifecycle`(協議契約,放進 arona)。** 只放 JSON-RPC 方法與型別:
  `DrainState`、`ReadyStatus`、`Lifecycle.Drain`、`Lifecycle.Status`、
  `Worker.Status` 等。符合 arona"雙方配對"的規則。
- **`malkuth`(新 crate,執行時)。** 依賴 `arona` 協議型別 +
  `tokio` + `libsystemd` 繫結(socket activation)+ 後端 trait。按 feature
  開啟:
  - `replica` —— 子系統 A 的協調與編排。
  - `leader-follower` —— 子系統 B 的租約選主與故障轉移。
  - `socket-activation` —— systemd fd 獲取。
  - `file-lock` / `pg-lock` / `lease` —— `CoordinationLock` 後端。
三個專案依賴 `malkuth`,按需開啟 feature(見 §8 矩陣)。把一切塞進
arona 會迫使它變成"協議 + 可選執行時",毀掉其純淨性——不推薦。
## 5. 核心抽象
### 5.1 `Worker` —— 一個被監督的子程序資源
一個 `Worker` 是一個可獨立殺死的程序,它恰好持有一個資源(一條 PLC 連線、
一個串列埠、一個本地監聽埠、一個如 cosmos / pglite-proxy 的 sidecar)。程序
即**故障隔離邊界**:Modbus 棧的 bug 不會汙染 S7comm worker。
生命週期狀態機(取自 `evernight/src/model_server.rs:128-139`):
```
        啟動                       健康檢查透過
 Starting ──────► Running ─────────────────► Running
     │              │  ▲
     │              │  │ 健康檢查透過(自愈)
     │              ▼  │
     └──────► Failed ◄┘        崩潰 / 不健康
                  │
                  │ 重啟策略 = permanent
                  └────────► Starting(受限頻控制)
```
### 5.2 `Supervisor` —— 持有 worker 池
- **重啟策略**(OTP 詞彙):`permanent`(總重啟——資源 worker 預設)、
  `transient`(僅異常退出才重啟)、`temporary`(從不重啟)。
- **滑動視窗限頻**(取自 entelecheia `health_daemon` 的
  `max_restart_attempts` + `cooldown`):若某 worker 在視窗 W 內重啟超過
  N 次,進入 `cooldown`,以防崩潰風暴;後續重啟被推遲。
### 5.3 `Lifecycle` —— 統一訊號語義(Layer 1)
採用 nginx/Go 約定:
| 訊號 | 語義 | 行為 |
|---|---|---|
| `SIGINT`(ctrl_c) | 等價 SIGTERM(對開發友好) | 進入排空 |
| `SIGTERM` | **優雅停機** | 排空:清 ready 位 → 停止 accept → 排空在途 → 退出 |
| `SIGHUP` | **熱過載配置** | 不退出;重讀配置 |
| `SIGQUIT` | **立即退出**(僅緊急) | 跳過排空,快速退出 |
**排空序列**(一份實現;各專案注入各自的"排空閉包"):
1. 置 `/readyz` 的 `draining = true`(LB / 編排器據此停止派發新流量)。
2. 停止 `accept` 新連線(socket activation 下:停止從繼承的 fd accept)。
3. 給存活 WebSocket 發 close 幀;帶超時 `DRAIN_TIMEOUT`(預設 30s,可配)
   等待在途請求。
4. 排空後臺任務(抄 entelecheia `TaskManager.stop_all` + `wait_all`)。
5. 乾淨斷開上游連線池(抄 shittim-chest `upstream_pool` 的乾淨斷開)。
6. 釋放鎖、清理臨時檔案 → 退出 0。
實現要點:axum 的 `axum::serve(listener,
app).with_graceful_shutdown(...)` 已支援排空;**關鍵是把 `SIGTERM` 接進去**
(今天只接了 `ctrl_c`)。參考:`entelecheia/.../shutdown.rs:17`、
`shittim-chest/.../api.rs:465`、`evernight/src/api/mod.rs:109`。
### 5.4 健康端點(統一)
探針分離(今天三專案不一致):
| 端點 | 語義 | 判定 |
|---|---|---|
| `/healthz`(liveness) | 程序活著 | 程序能響應即 200(簡單的重啟判據) |
| `/readyz`(readiness) | **能接活**,帶 drain 位 | 未排空且依賴就緒(DB ping / scepter 套接字 / 站點首輪輪詢)→ 200;排空中 → 503 |
`/readyz` 的 `draining` 位是滾動更新的核心訊號:編排器只把新請求路由到
`/readyz` 為 200 的例項。shittim-chest 現有 `GET /api/health`
(`routes.rs:27`)升級為帶 drain 位的 `/readyz` 即可。
### 5.5 `acquire_listener` —— Layer 3 零停機交接
`malkuth` 暴露 `acquire_listener(addr) -> TcpListener`:
1. 優先 `sd_listen_fds()`(校驗 `LISTEN_PID`)—— systemd 持有 fd。
2. 回退到普通 `TcpListener::bind(addr)`(dev、無 systemd)。
axum `serve(listener, ...)` 本就接受預繫結 listener,管道齊備;今天只差 fd
的*來源*。三種部署適配:
| 部署 | 方案 | 適用 |
|---|---|---|
| **裸 systemd** | `xxx.socket` + `xxx@.service` 模板例項化 | scepter、evernight-gateway、malkuth 自身 |
| **docker**(shittim-chest 生產) | 宿主機 systemd socket activation,把已繫結的套接字/fd 透傳進容器(`LISTEN_FDS` + `SocketUser`);或容器內跑一個輕量 master 持 fd | shittim-chest 生產 |
| **dev** | 回退普通 `bind` + 短暫重疊(接受幾百 ms 丟連線),無 systemd | 三專案 dev |
socket activation 下的滾動更新:
```
[升級觸發] → 啟動新例項(模板化 service@new,繼承/重拿 fd)
            → 輪詢新例項 /readyz 直到 200
            → 給舊例項發 SIGTERM(= 排空)
            → 舊例項排空後自退
            → systemd 全程持有 fd → 零丟連線
```
### 5.6 `CoordinationLock` —— Layer 2 trait 與後端
滾動更新窗口裡,新舊例項可能併發讀寫共享資源。DB 事務天然安全;檔案
(evernight JSONL、配置)需要"寫前通知 + 上鎖"。
```rust
pub trait CoordinationLock: Send + Sync {
    async fn acquire(&self, key: &str, lease: Duration) -> Result<LockGuard>;
}
// 後端:
//   FileLock  — flock/fcntl,        適配 evernight(JSONL / 配置)
//   PgLock    — pg_advisory_lock,   適配 entelecheia / shittim-chest
//   LeaseLock — 檔案鎖 + 租約(崩潰自動過期)
```
**例項登錄檔**(僅在升級視窗使用;常態單條記錄):一個小的共享表/檔案,記錄
`{instance_id, role: Active | Draining, started_at, generation}`。新例項
啟動寫一條 `Active`;升級時把舊的標 `Draining`。它替代了被明確排除在外的
Raft 仲裁——因為常態單例項,登錄檔只用於協調排空,無強一致需求(一個檔案
或 DB 行即可)。
## 6. 子系統 A —— 副本(負載平衡 ⊃ 滾動更新)
**形態。** N 個相同對等例項在前置 LB 後並行執行,狀態在共享 Postgres。
**active-active**,例項對等,無主。
| 關切點 | 方案 |
|---|---|
| 請求路由 | 前置 LB(caddy / 內建 `SO_REUSEPORT` 輪詢);按 `/readyz` 摘除 |
| 共享狀態讀寫 | DB 事務 + `pg_advisory_lock` 協調併發寫(天然安全) |
| WebSocket / 長連線 | **粘性會話**(LB 按 cookie/例項 id 固定)或**會話遷移**(斷開後客戶端重連到任一副本,狀態從 DB 恢復——shittim-chest `upstream_pool` 已是重連模板) |
| 會話親和 | entelecheia 與 shittim-chest 都把狀態外接到 Postgres 且啟動即恢復 → **天然副本友好**,這是它們相對 evernight 的巨大優勢 |
| 滾動更新 | 副本伸縮的子操作:加新副本(新版本)→ ready → 排空並移除舊副本 → 重複。即 K8s `maxSurge/maxUnavailable` 的微縮版 |
**為什麼 A 相對簡單。** 因為 entelecheia 與 shittim-chest 把狀態外接到
Postgres(啟動恢復,審計已確認),副本間無需複製狀態——前置 LB 加 DB 事務
就夠。唯一難點是 WebSocket 的粘性 / 遷移。
## 7. 子系統 B —— 主備(邊緣 active-passive HA)
**形態。** 同一臺裝置/閘道器上兩個 evernight 程序,一主一備;上游
`evernight-server` 看到的是**同一個 `node_id`**(一個裝置)。
**active-passive**,例項不對等,主獨佔物理 I/O。
### 7.1 簡化 B 的"取巧":監督樹 + let-it-crash
不要給每個資源都做容錯,**只讓 supervisor 做主備**;資源是獨立的 worker 進
程,掛了直接重啟。具體(按已定決策):
```
supervisor  (主 / 備 HA)              ← 只有這層做租約選主 + 故障轉移
   ├─ worker: PLC-A(Modbus)           ← 監督重啟,單例項
   ├─ worker: PLC-B(S7comm)           ← 監督重啟,單例項
   ├─ worker: 串列埠 / CAN               ← 監督重啟,單例項
   └─ worker: 本地埠監聽             ← 監督重啟,單例項
```
這就是 OTP 監督樹 / "let it crash" 模型(也是 systemd `Restart=always`、
K8s Pod)。收益:
- **關注點分離。** worker 只"持有一個資源、幹活",不背容錯、選主、狀態同步
  邏輯——保持極簡。
- **容錯集中。** 只有 supervisor 做 HA;複雜度收斂到一處。
- **故障隔離。** 某個資源(如某 PLC 協議)崩潰不影響其他(獨立程序)。
- **契合協議多樣性。** evernight 說多種工業協議(Modbus/S7/CAN/串列埠);每個
  對應一個 worker,隔離價值最大。
### 7.2 故障轉移時 worker 的處理 —— 子程序模型(已定起步方案)
worker 是 supervisor 的**子程序**(`kill_on_drop`)。主掛 → worker 成孤兒/
被殺 → 提升後的備**重新拉起全部 worker**(每條 PLC 重連)。
- 最簡單的模型;契合"服務端負責"的意圖。
- 代價:supervisor 故障轉移時,所有資源短暫中斷重連。
- 進階(延後):worker 作為更底層 init 下的獨立常駐程序,supervisor 只通過
  IPC 指揮;新 supervisor `attach` 存活的 worker。中斷更小,但 worker 需實現
  "重新認主"——更復雜。作為進階選項記錄在案。
### 7.3 選主 + fencing
- **租約選主。** 主持有檔案鎖 + 租約(帶 TTL),每次心跳續租;備輪詢;主心跳
  超時 → 備奪租約 → 自我提升。
- **排他物理資源。** PLC/串列埠/CAN 連線只能由主持有(兩程序同時 poll 同一
  PLC 會衝突)→ 主 poll,備待命。這是 B 必須 active-passive 而非
  active-active 的根本原因。
- **狀態同步。** 起步**冷備**(備不復制;提升時從盤上 JSONL 恢復)。熱備
  (備追主的 JSONL 日誌)為進階選項。
- **腦裂 fencing。** 租約 TTL + fencing:備必須在租約真正過期後才能奪主;奪
  主後,舊主被物理阻止再寫(物理 I/O 獨佔性是天然 fence)。
- **單一裝置身份。** 主備共用一個 `node_id`;只有當前主發 `device.register`。
經典對照:keepalived/VRRP、DRBD+Pacemaker、MySQL 主從、Redis Sentinel——
作為單機內、程序級的簡化版。理論(租約選主 + fencing)基礎紮實。
## 8. 各專案適配矩陣
| 專案 | supervisor 角色 | worker 們 | 策略 |
|---|---|---|---|
| entelecheia(scepter) | 副本之一 | cosmos sidecar、agent 容器 | **A 副本** |
| shittim-chest(chest) | 副本之一 | pglite-proxy(mock)、channel intake | **A 副本** |
| evernight 裝置(`sensor-poll`) | **主 / 備** | 每協議一個 worker(Modbus/S7/CAN/串列埠) | **B 主備** |
| evernight-server(中心) | 副本之一 | model_server 容器 | **A 副本** |
各專案的 feature 選擇(`malkuth`):
- entelecheia / shittim-chest / evernight-server:`replica` +
  `socket-activation` + `pg-lock`;其 sidecar 走 worker 抽象。
- evernight 裝置:`leader-follower` + `socket-activation`(進階 fd 接管)+
  `file-lock` / `lease`;每協議程序走 worker 抽象。
## 9. 落地階段
1. **階段 A —— Layer 1 跨三專案落地。** 訊號語義 + `/healthz` / `/readyz`
   + 排空。風險最低,立竿見影(先把 SIGTERM 硬殺修掉)。
2. **階段 B —— `arona::lifecycle` 協議 + `malkuth` 骨架。** trait
   定義、`acquire_listener`、`CoordinationLock` trait + `FileLock` /
   `PgLock` 後端、`Worker` + `Supervisor` 原語。
3. **階段 C —— Layer 3。** 三專案的 socket activation 單元 + docker 適配 +
   dev 回退。
4. **階段 D —— Layer 4。** manifest 佇列編排器(抄 `health_daemon`)+ 新舊
   並存排空閉環,含 dev"編譯新服務端"流程;以及 B 的主備故障轉移。
## 10. 風險與邊界
- **shittim-chest docker + socket activation** 是最不確定的一環;fd 透傳進
  容器需做原型驗證。若不可行,退化為"外部 caddy + `/readyz` 摘除 + 短暫
  重疊"。
- **evernight 記憶體態**(`DeviceRegistry`、會話)排空時會丟;需評估是否落盤
  或遷移(最壞情況:新例項重建,長會話斷)。
- **WebSocket 排空 ≠ 遷移。** 舊例項退出時,在途 WS 仍會斷;"無縫"要求客戶
  端重連到新例項(shittim-chest 客戶端已有 `upstream_pool` 重連邏輯,可複用)。
- **程序級 vs 執行緒級容錯。** 主備(子系統 B)解決的是*程序/例項級*容錯,不是
  執行緒級。tokio 任務內 panic 是 supervisor 的活(任務重啟,而非程序故障轉
  移)。不要用 B 去扛執行緒崩潰——那太重了。
- **明確不在範圍。** Raft 仲裁、一致雜湊分片、跨機房 HA——已被"滾動更新 +
  邊緣 HA"的範圍排除。常態單例項意味著登錄檔只用於協調排空。
## 11. 待定問題(延後)
- 故障轉移時的 worker:起步選子程序模型;"獨立常駐 + attach"模型作為進階記
  錄在案。
- B 的冷備 vs 熱備:起步選冷備(提升時從 JSONL 恢復);熱備(備追主日誌)延
  後。
- 是否把 entelecheia 的 `cosmos` sidecar 與 shittim-chest 的
  `pglite-proxy` 也納入統一 `Worker` 抽象:已同意(三專案統一進同一套
  worker 抽象)。
---
*對應英文權威源:`docs/en/design/platform/supervision-and-rolling-update.md`。
其他語言(ja/ko/fr/es/ru)的翻譯待 i18n。*
