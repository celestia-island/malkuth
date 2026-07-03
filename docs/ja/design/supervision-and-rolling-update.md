# 統合スーパビジョン・ローリングアップデート・レプリケーションアーキテクチャ
> **スコープ。** 本文は*プラットフォームレベル*の設計であり、`core`
> (entelecheia / scepter)、`webui` (shittim-chest / chest)、`router`
> (evernight) を横断する。各プロジェクトごとのアーキテクチャ文書はそれぞれの
> `core/`、`webui/`、`router/` サブカテゴリに置かれており、本文はその三者が
> すべて利用する共有ライフサイクル / スーパビジョン層を定義する。
## 1. 背景と目標
三つのプロジェクトは構造的に均質である——いずれも **Rust (edition 2024,
MSRV 1.85) + axum 0.8 + tokio + Unix ソケット / WebSocket 経由の JSON-RPC**
であり、プロトコル層としてすでに `arona` crate を共有している。この均質性
こそが、*単一の*スーパビジョン機構を一度だけ作って三度再利用する価値を
もたらす。
この機構は、重なり合う四つのニーズを一つのコヒーレントな機能として提供
しなければならない:
1. **ロードバランシング** —— 同一プログラムの複数の同一インスタンスを
   並行して稼働させ、データベース / 設定 / ランタイム状態を共有したまま
   IPC で協調しながら処理を分担する。
2. **協調書き込み** —— あるインスタンスが共有ファイルを書き換えようとする
   際、他のインスタンスに通知してロックを取得しなければならない。並行書き
   換えで状態を破壊しないためである。
3. **ローリングアップデート** —— 新しい公式リリース（あるいはローカルで
   コンパイルしたばかりのデバッグサーバ）が届いたとき、新旧バイナリが共存
   できる。旧プロセスは進行中の処理を終えてから終了し、稼働セットを新
   プロセスに引き継ぐ。
4. **エッジのフォールトトレランス** —— デバイス上（とくに evernight
   ゲートウェイ）では二つのプロセスをリーダ/フォロワで動かし、一方のクラッシュ
   がデバイス全体を落とさないようにする。
### 1.1 現状（本設計が埋めるギャップ）
コード監査の結果、**三つのプロジェクトすべて**に同一の三つの欠陥が見つ
かった:
| 能力 | entelecheia (scepter) | shittim-chest (chest) | evernight |
|---|---|---|---|
| シグナル処理 | `ctrl_c` のみ (`shutdown.rs:17`) | `ctrl_c` のみ (`api.rs:465`) | `ctrl_c` のみ (`api/mod.rs:109`) |
| ドレインロジック | HTTP はドレインするが WS / バックグラウンドタスクはしない | 同左 | なし |
| リスナ fd の引き継ぎ | なし | なし | なし |
| drain ビット付き `/readyz` | なし | `/api/health` はあるが drain ビットなし | なし |
最大の問題は: `SIGINT` しか捕捉していないため、`docker stop` /
`systemctl restart` が送る **`SIGTERM`** を受けるとグレースフルシャットダウン
経路が完全にバイパスされ、猶予期間のあとハードキルされる。これを修正する
だけでも、単一の修正として最もレバレッジが高い。
### 1.2 再利用できる既存アセット
- `entelecheia/packages/cli/src/evernight_daemon.rs` —— 現在最も完成度の高い
  自己再起動のブループリント。PID ロックファイル + 自己 reexec + レディ待ち +
  `SIGTERM`→`SIGKILL` フォールバックを備える。
- `entelecheia/packages/scepter/src/daemon/health_daemon.rs` —— **JSON
  manifest ファイルキュー**によるコンテナローリングアップデート（言語非依存の
  アップデートプリミティブ）。
- `entelecheia/packages/shared/infra_jsonrpc` —— Unix ソケット JSON-RPC トランスポート層。
- `shittim-chest/packages/core/src/proxy/upstream_pool.rs` —— 指数バックオフ
  再接続付きマルチエンドポイントレジストリ。ロードバランシングクライアントの
  すぐれたテンプレート。
- `evernight/src/model_server.rs` —— `Running/Starting/Stopped/Failed` リソース
  ライフサイクル状態機械で、deploy→wait-health→stop-old を備える。リポジトリ内
  のアプリケーション層ローリングアップデートのテンプレート。
## 2. 理論的基盤
この機構は単一の理論ではなく、それぞれに標準的な実装をもつ、よく確立された
産業パターンの組み合わせである。それらを明示的に名前挙げすることで、設計は
検証済みのセマンティクスを引き継ぐ:
| 表現されるニーズ | 産業用語 | 標準的実装 |
|---|---|---|
| 新旧プロセスが共存し、旧プロセスは進行中の処理を終えてから終了 | **グレースフルシャットダウン / ドレイン + ローリングアップデート** | Kubernetes Deployment、nginx / unicorn |
| "赤/青マーカ"（議論で思い出された名称） | **ブルーグリーンデプロイ**（二つの並行環境を立て、トラフィックのポインタを切り替える）。議論で語られた漸減挙動は、ブルーグリーンよりも**ローリングアップデート + ドレイン**に近い。 | |
| 新プロセスが同一ポートを接続を落とさずに引き継ぐ | **socket activation / fd 継承 / `SO_REUSEPORT`** | systemd、nginx `USR2`、envoy hot restart |
| "書く前に相手へ通知してロックする" | **アドバイザリロック (`flock`/`fcntl`) / DB 行ロック / リース** | POSIX アドバイザリロック、`pg_advisory_lock` |
| プロセスの自己修復 | **スーパビジョンツリー (OTP) / systemd / kubelet** | Erlang/OTP、systemd、s6、immortal |
| リーダ/フォロワでクラッシュがサービス全体を落とさない | **リースベースのリーダ選出 + フェンシング** | Chubby リース、Raft リーダ選出（部分集合）、keepalived/VRRP、Pacemaker |
| リソースを一つのプロセスが保持し、クラッシュ時に再起動 | **"let it crash" + スーパバイザによる再起動** | Erlang/OTP スーパビジョンツリー、systemd `Restart=always` |
**ローリングアップデートの産業標準レシピ**（そのまま踏襲する価値がある）は
Kubernetes のものである:
`maxSurge + maxUnavailable + readinessProbe + preStop hook + SIGTERM
グレースフルシャットダウン + 猶予期間 + PodDisruptionBudget`。我々が作るのは
そのセルフホスト、シングルホスト / 小クラスタ版である。
**nginx のホットアップグレード**は「新旧が共存し、その後旧をドレインする」の
教科書である: `USR2` が listen fd を継承する新 master を起動 → `WINCH` が
旧 worker をグレースフルに停止 → `QUIT` が旧 master を退役させる。§7 の
フローは構造的にこれと同一である。
## 3. 全体アーキテクチャ
設計は単一のエレガントなバックボーンに収束する:**どこでもスーパビジョン
ツリー。唯一の違いは、スーパバイザがピアレプリカなのかリーダ/フォロワなの
かだけである。**
```
                     ┌─────────────────────────────────────────┐
    共有ベース        │ シグナルセマンティクス · ドレイン         │  ← 三プロジェクトすべて
    (Layer 1 / 3)    │ /healthz · /readyz (drain ビット付き)    │     完全に同一
                     │ socket activation + 3 種のデプロイアダプタ │
                     └─────────────────────────────────────────┘
                                    │
           ┌────────────────────────┴────────────────────────┐
           ▼                                                  ▼
    ┌────────────────┐                              ┌──────────────────┐
    │ サブシステム A  │  スーパバイザはピア          │ サブシステム B    │  スーパバイザは
    │ Replica        │  (active-active)             │ Leader/Follower  │  leader または follower
    │                │  ← サーバ LB + ローリング更新 │                  │  ← エッジフォールトトレランス
    └───────┬────────┘                              └─────────┬────────┘
            │                                                  │
            └──────────────────┬───────────────────────────────┘
                               ▼
                ┌──────────────────────────────┐
                │ 統一 Worker 抽象             │  ← 三プロジェクトすべての「子プロセスリソース」
                │ ライフサイクル FSM + 監視下   │     cosmos / pglite-proxy / プロトコル別 worker
                │ 再起動 (permanent/transient) │
                │ + スライディングウィンドウ速率制限 │
                └──────────────────────────────┘
```
### 3.1 鍵となる単純化: ローリングアップデートは*第三のシステムではない*
スーパバイザツリーがバックボーンになれば、ユーザのニーズは三つではなく
**二つの**サブシステムに収束する:
- **サブシステム A —— Replica。** サーバサイドで N 個の同一ピアインスタンスを
  稼働させ負荷を分担する。*ロードバランシングとローリングアップデートは同一
  サブシステムである:* ローリングアップデートは「レプリカ数を一時的に +1 →
  古いレプリカを一つドレイン → 繰り返す」にすぎない。Kubernetes の
  `maxSurge/maxUnavailable` 操作を、レプリカの追加/削除で表現したものだ。
- **サブシステム B —— Leader/Follower。** エッジ (evernight デバイス) では
  フォールトトレランスのために二つのプロセスを動かす。一方がリーダ、もう一方が
  フォロワである。リーダが物理 I/O を排他的に所有し、フォロワは待機する。
独立した「ローリングアップデートシステム」は存在しない: それはサブシステム A の
メンテナンス操作であり（意味は異なるが、B でもリーダフェイルオーバ経由で作用する）。
### 3.2 レイヤビュー
| レイヤ | サブシステム A (Replica) | サブシステム B (Leader/Follower) | 共通か? |
|---|---|---|---|
| **L1** ライフサイクル (シグナル / ドレイン / プローブ) | 同じ | 同じ | **共通** |
| **L3** ゼロダウンタイム引き継ぎ (socket activation) | 各レプリカは systemd から fd を得る | leader→follower の fd 引き継ぎ (高度) | **共通** |
| **L2** 協調 | **2a** ピアレジストリ + 共有ロック (`pg_advisory`) | **2b** リース選出 + 排他的リソース + リーダ/フォロワレジストリ | **フォーク** (同一 trait、異なるポリシー) |
| **L4** オーケストレーション | **4a** レプリカのスケール / ローリングアップデート | **4b** フェイルオーバ | **フォーク** |
洞察: **L1 と L3 は完全に共通; L2/L4 はフォークする。** `CoordinationLock` は
2a と 2b で同一 trait である——A では並行書き込みの協調に、B ではリーダリースに
使われる。この trait の統一こそが、「原理は共通である」が着地する場所である。
## 4. crate の帰属
ユーザ目標である「arona に入れる」は分割しなければならない。なぜなら、**arona
は現在純粋なプロトコル/型 crate である**——`serde` / `ts-rs` / `schemars` への
依存しかなく、`lib.rs:5` は「すべての型は entelecheia で定義され、shittim-chest
が消費する」を必須とし、crates.io 公開のためにすべての非プロトコル成果物を
`exclude` している。ランタイムロジック (tokio、`sd_listen_fds`、シグナル処理)
を注入すれば、この軽量で公開可能なアイデンティティを壊すことになる。
分割:
- **`arona::lifecycle` (プロトコル契約、arona に置く)。** JSON-RPC のメソッドと
  型のみ: `DrainState`、`ReadyStatus`、`Lifecycle.Drain`、`Lifecycle.Status`、
  `Worker.Status` など。arona の「両サイドでペアになる」というルールを満たす。
- **`malkuth` (新規 crate、ランタイム)。** `arona` プロトコル型 +
  `tokio` + `libsystemd` バインディング (socket activation) + バックエンド trait
  に依存する。feature ゲート:
  - `replica` —— サブシステム A の協調とオーケストレーション。
  - `leader-follower` —— サブシステム B のリース選出とフェイルオーバ。
  - `socket-activation` —— systemd からの fd 取得。
  - `file-lock` / `pg-lock` / `lease` —— `CoordinationLock` バックエンド。
三つのプロジェクトは `malkuth` に依存し、必要な feature を有効化する
(§8 のマトリクスを参照)。すべてを arona に押し込めば、arona を「プロトコル +
オプションのランタイム」に変えることになり、その純粋性を破壊する——推奨しない。
## 5. コア抽象
### 5.1 `Worker` —— 監視下の子プロセスリソース
`Worker` は、厳密に一つのリソース (PLC 接続、シリアルポート、ローカルリスン
ポート、cosmos / pglite-proxy のようなサイドカー) を保持する、独立に kill 可能な
一つのプロセスである。プロセスが**障害分離境界**である: Modbus スタックのバグが
S7comm worker を汚染することはない。
ライフサイクル FSM (`evernight/src/model_server.rs:128-139` から流用):
```
         start                      health ok
  Starting ──────► Running ─────────────────► Running
      │              │  ▲
      │              │  │ health ok (自己修復)
      │              ▼  │
      └──────► Failed ◄┘        クラッシュ / 不健康
                   │
                   │ restart policy = permanent
                   └────────► Starting (速率制限付き)
```
### 5.2 `Supervisor` —— worker プールを所有
- **再起動ポリシー** (OTP の語彙): `permanent` (常に再起動——リソース worker の
  デフォルト)、`transient` (異常終了時のみ再起動)、`temporary` (再起動しない)。
- **スライディングウィンドウ速率制限** (entelecheia の `health_daemon` の
  `max_restart_attempts` + `cooldown` から流用): worker がウィンドウ W 内に N 回
  を超えて再起動すると、クラッシュストームを防ぐため `cooldown` に入り、以降の
  再起動は保留される。
### 5.3 `Lifecycle` —— 統一シグナルセマンティクス (Layer 1)
nginx/Go の慣習を採用する:
| シグナル | セマンティクス | 挙動 |
|---|---|---|
| `SIGINT` (ctrl_c) | SIGTERM と同等 (開発者向けに親切) | ドレインに入る |
| `SIGTERM` | **グレースフルシャットダウン** | ドレイン: ready ビットをクリア → accept 停止 → 進行中の処理をドレイン → 終了 |
| `SIGHUP` | **ホット設定リロード** | 終了せず、設定を再読み込み |
| `SIGQUIT` | **即時終了** (緊急時のみ) | ドレインをスキップ、高速終了 |
**ドレインシーケンス** (一実装例; 各プロジェクトは独自の「ドレインクロージャ」を
注入する):
1. `/readyz` の `draining = true` をセット (LB / オーケストレータがこれを見て
   新しいトラフィックの送信を止める)。
2. 新しい接続の `accept` を停止 (socket activation 下では: 継承した fd からの
   accept を停止)。
3. 生存中の WebSocket へ close フレームを送信; タイムアウト `DRAIN_TIMEOUT`
   (デフォルト 30s、設定可能) で進行中のリクエストを待つ。
4. バックグラウンドタスクをドレイン (entelecheia の `TaskManager.stop_all` +
   `wait_all` を踏襲)。
5. 上流プールをきれいに切断 (shittim-chest の `upstream_pool` のきれいな切断を
   踏襲)。
6. ロックを解放し、一時ファイルをクリーンアップ → exit 0。
実装メモ: axum の `axum::serve(listener,
app).with_graceful_shutdown(...)` はすでにドレインをサポートしている。**欠けて
いる鍵のピースは `SIGTERM` を接続することだ** (現状では `ctrl_c` しか接続されて
いない)。参照: `entelecheia/.../shutdown.rs:17`、
`shittim-chest/.../api.rs:465`、`evernight/src/api/mod.rs:109`。
### 5.4 ヘルスエンドポイント (統一)
プローブを分割する (現状、三プロジェクトは不整合):
| エンドポイント | セマンティクス | 判定 |
|---|---|---|
| `/healthz` (liveness) | プロセスが生きている | プロセスが応答できれば 200 (シンプルな再起動基準) |
| `/readyz` (readiness) | **要求を処理できる**、drain ビットを保持 | ドレイン中でなく、かつ依存先が稼働 (DB ping / scepter ソケット / 最初のステーションポーリング) なら 200; ドレイン中は 503 |
`/readyz` の `draining` ビットはローリングアップデートの中心的シグナルである:
オーケストレータは新しいリクエストを `/readyz` が 200 のインスタンスにだけ
ルーティングする。shittim-chest の既存の `GET /api/health` (`routes.rs:27`)
は drain ビット付きの `/readyz` にアップグレードされる。
### 5.5 `acquire_listener` —— Layer 3 ゼロダウンタイム引き継ぎ
`malkuth` は `acquire_listener(addr) -> TcpListener` を公開する:
1. まず `sd_listen_fds()` を試す (`LISTEN_PID` を検証) —— systemd が fd を保持
   している。
2. フォールバックとして普通の `TcpListener::bind(addr)` (dev、systemd なし)。
axum の `serve(listener, ...)` はすでに事前バインド済みのリスナを受け付けるので、
配管は存在する; 現状で欠けているのは fd の*ソース*だけである。三つのデプロイ
アダプタ:
| デプロイ | 方式 | 対象 |
|---|---|---|
| **bare systemd** | `xxx.socket` + `xxx@.service` テンプレートインスタンス | scepter、evernight-gateway、malkuth 自身 |
| **docker** (shittim-chest 本番) | ホスト側 systemd で socket activation し、バインド済みのソケット/fd をコンテナへ受け渡し (`LISTEN_FDS` + `SocketUser`); もしくはコンテナ内に fd を保持する軽量 master を立てる | shittim-chest 本番 |
| **dev** | フォールバックとして普通の `bind` と短いオーバーラップ (数百 ms のドロップ接続を受け入れる)、systemd なし | 三プロジェクトの dev |
socket activation 下でのローリングアップデート:
```
[アップグレードトリガ] → 新インスタンス起動 (テンプレ化 service@new、fd を継承/再取得)
                       → 新インスタンスの /readyz が 200 になるまでポーリング
                       → 旧インスタンスへ SIGTERM (= ドレイン)
                       → 旧インスタンスがドレインして自ら終了
                       → systemd が全程で fd を保持 → 接続ドロップゼロ
```
### 5.6 `CoordinationLock` —— Layer 2 trait とバックエンド
ローリングアップデートの窓の間、新旧のインスタンスが共有リソースを並行に
読み書きする可能性がある。DB トランザクションは自然に安全だが、ファイル
(evernight の JSONL、設定) には「書き込み前の通知 + ロック」が必要である。
```rust
pub trait CoordinationLock: Send + Sync {
    async fn acquire(&self, key: &str, lease: Duration) -> Result<LockGuard>;
}
// バックエンド:
//   FileLock  — flock/fcntl,        evernight 用 (JSONL / 設定)
//   PgLock    — pg_advisory_lock,   entelecheia / shittim-chest 用
//   LeaseLock — ファイルロック + リース (クラッシュで自動失効)
```
**インスタンスレジストリ** (アップグレードの窓の間だけ使用; 定常状態では
単一レコード): `{instance_id, role: Active | Draining, started_at, generation}` を
記録する小さな共有テーブル/ファイル。新インスタンスは起動時に `Active` 行を
書き込み、アップグレード時には旧を `Draining` にマークする。これは明示的に
スコープ外とされた Raft クォーラムを置き換える——定常状態は単一インスタンス
であるため、レジストリはドレインの協調のみを行い、強い一貫性の必要はない
(ファイルか DB 行で十分)。
## 6. サブシステム A —— Replica (ロードバランシング ⊃ ローリングアップデート)
**形態。** 前面の LB の後ろで N 個の同一ピアイインスタンスが並行稼働し、状態は
共有 Postgres にある。**active-active**、ピアは対等、リーダはいない。
| 関心事 | 方式 |
|---|---|
| リクエストルーティング | 前面の LB (caddy / ビルトインの `SO_REUSEPORT` ラウンドロビン); `/readyz` による除外 |
| 共有状態の R/W | DB トランザクション + 並行書き込み協調用の `pg_advisory_lock` (自然に安全) |
| WebSocket / ロングコネクション | **スティッキーセッション** (LB が cookie/instance id で固定) もしくは**セッションマイグレーション** (切断後クライアントが任意のレプリカへ再接続、状態は DB から復元——shittim-chest の `upstream_pool` はすでに再接続のテンプレート) |
| セッションアフィニティ | entelecheia と shittim-chest はいずれも状態を Postgres に外部化し起動時に復元する → **自然にレプリカフレンドリ**、これが evernight に対する両者の利点である |
| ローリングアップデート | レプリカスケールのサブ操作: 新レプリカ (新バージョン) を追加 → ready → 古いレプリカをドレイン & 削除 → 繰り返す。K8s `maxSurge/maxUnavailable` のミニチュア版 |
**なぜ A は比較的容易か。** entelecheia と shittim-chest は状態を Postgres に
外部化しているため (監査で確認された通り、起動時に復元)、レプリカ間での状態
レプリケーションは不要である——前面の LB と DB トランザクションだけで十分だ。
唯一の難所は WebSocket のスティッキー性 / マイグレーションである。
## 7. サブシステム B —— Leader/Follower (エッジの active-passive HA)
**形態。** 同一デバイス/ゲートウェイ上の二つの evernight プロセス、一方が
リーダ、もう一方がフォロワ。上流の `evernight-server` は**一つの `node_id`**
(一つのデバイス) として見る。**active-passive**、インスタンスは対等でなく、
リーダが物理 I/O を排他的に所有する。
### 7.1 B を単純化する「トリック」: スーパビジョンツリー + let-it-crash
すべてのリソースをフォールトトレラントにするのではなく、**スーパバイザだけを
リーダ/フォロワにする**; リソースは単にクラッシュ時に再起動される独立した
worker プロセスである。具体的には (合意された決定に基づく):
```
supervisor  (leader / follower HA)        ← このレイヤだけがリース選出 + フェイルオーバを行う
   ├─ worker: PLC-A (Modbus)              ← 監視下再起動、単一インスタンス
   ├─ worker: PLC-B (S7comm)              ← 監視下再起動、単一インスタンス
   ├─ worker: serial / CAN                ← 監視下再起動、単一インスタンス
   └─ worker: ローカルポートリスナ        ← 監視下再起動、単一インスタンス
```
これは OTP スーパビジョンツリー / "let it crash" モデルである (systemd
`Restart=always`、K8s Pod でも同じ)。利点:
- **関心の分離。** worker は「一つのリソースを保持して処理を行う」だけであり、
  フォールトトレランス、選出、状態同期のロジックを担わない——極めてシンプルに
  保たれる。
- **フォールトトレランスの集中。** スーパバイザだけが HA を担う; 複雑さが一箇所に
  収束する。
- **障害の隔離。** 一つのリソース (例: ある PLC プロトコル) のクラッシュが他に
  影響を与えない (別プロセス)。
- **プロトコルとの適合。** evernight は多くの産業プロトコル (Modbus/S7/CAN/serial)
  を扱う; それぞれを worker に割り当てることで、隔離の価値が最大化される。
### 7.2 フェイルオーバ時の worker ライフサイクル —— 子プロセスモデル (合意された出発点)
worker はスーパバイザの**子プロセス** (`kill_on_drop`) である。リーダの死亡 →
worker は孤児/kill される → 昇格したフォロワが**全 worker を再 spawn する**
(各 PLC が再接続する)。
- 最もシンプルなモデル; 「サーバが責任をもつ」という意図に合致する。
- コスト: スーパバイザのフェイルオーバ時に、すべてのリソースが短時間ドロップ
  して再接続する。
- 高度 (保留): worker をより低レベルの init 配下の独立したデーモンとし、
  スーパバイザは IPC 経由で指揮のみを行う; 新スーパバイザが生存 worker を
  `attach` する。中断は小さいが、worker は「現スーパバイザへ再 bind」を
  実装しなければならない——より複雑。将来の選択肢として記録。
### 7.3 リーダ選出 + フェンシング
- **リース選出。** リーダはファイルロック + リース (TTL 付き) を保持し、
  ハートビートごとに更新する; フォロワはポーリングする; リーダのハートビートが
  タイムアウトすると、フォロワがリースを奪取し自身を昇格させる。
- **排他的な物理リソース。** PLC/serial/CAN 接続はリーダだけが保持できる
  (二つのプロセスが同じ PLC をポーリングすれば衝突する) → リーダが poll し、
  フォロワは待機する。これが B が active-active ではなく active-passive でなければ
  ならない根本理由である。
- **状態同期。** 出発点は**コールドスタンバイ** (フォロワはレプリケートしない;
  昇格時にオンディスクの JSONL から復元する)。ホットスタンバイ (フォロワが
  リーダの JSONL を tail する) は高度な選択肢。
- **スプリットブレインのフェンシング。** リース TTL + フェンシング: フォロワは
  リースが本当に失効したあとにのみ奪取できる; 奪取後、旧リーダのさらなる書き込みは
  物理的に阻止される (物理 I/O の排他性が自然なフェンスである)。
- **単一デバイスアイデンティティ。** リーダとフォロワは一つの `node_id` を共有する;
  現リーダだけが `device.register` を発行する。
古典的な類似: keepalived/VRRP、DRBD+Pacemaker、MySQL プライマリ/レプリカ、
Redis Sentinel —— これらのマシン内、プロセスレベルの簡略版である。理論
(リース選出 + フェンシング) は堅牢である。
## 8. プロジェクト別導入マトリクス
| プロジェクト | スーパバイザの役割 | worker | 戦略 |
|---|---|---|---|
| entelecheia (scepter) | レプリカの一つ | cosmos サイドカー、agent コンテナ | **A Replica** |
| shittim-chest (chest) | レプリカの一つ | pglite-proxy (mock)、channel intake | **A Replica** |
| evernight デバイス (`sensor-poll`) | **リーダ / フォロワ** | プロトコルごとに一つの worker (Modbus/S7/CAN/serial) | **B Leader/Follower** |
| evernight-server (中央) | レプリカの一つ | model_server コンテナ | **A Replica** |
プロジェクトごとの feature 選択 (`malkuth`):
- entelecheia / shittim-chest / evernight-server: `replica` +
  `socket-activation` + `pg-lock`; 各サイドカーは worker 抽象を使用。
- evernight デバイス: `leader-follower` + `socket-activation` (高度な fd
  引き継ぎ) + `file-lock` / `lease`; プロトコル別プロセスは worker 抽象を使用。
## 9. ロールアウトのフェーズ
1. **フェーズ A —— 三プロジェクト全体への Layer 1。** シグナルセマンティクス +
   `/healthz` / `/readyz` + ドレイン。リスクが最も低く、即効性が最も高い
   (まず SIGTERM のハードキルを修正する)。
2. **フェーズ B —— `arona::lifecycle` プロトコル + `malkuth`
   スケルトン。** trait 定義、`acquire_listener`、
   `CoordinationLock` trait + `FileLock` / `PgLock` バックエンド、`Worker` +
   `Supervisor` プリミティブ。
3. **フェーズ C —— Layer 3。** 三プロジェクトの socket activation ユニット +
   docker アダプタ + dev フォールバック。
4. **フェーズ D —— Layer 4。** manifest キューオーケストレータ (`health_daemon`
   を踏襲) + 新旧共存ドレインループ。dev の「新サーバをコンパイル」ワークフロー
   を含む; 加えて B のリーダ/フォロワフェイルオーバ。
## 10. リスクと境界
- **shittim-chest docker + socket activation** が最も不確実なピースである;
  コンテナへの fd 受け渡しにはプロトタイプのスパイクが必要。実現不能な場合は、
  「外部 caddy + `/readyz` による除外 + 短いオーバーラップ」へフォールバックする。
- **evernight のメモリ内状態** (`DeviceRegistry`、セッション) はドレイン時に
  失われる; 永続化またはマイグレーションを行うかを決める必要がある (最悪の場合:
  新インスタンスが再構築し、長いセッションはドロップする)。
- **WebSocket のドレイン ≠ マイグレーション。** 旧インスタンスの終了時、進行中の
  WS はやはりドロップする; 「シームレス」にはクライアントが新インスタンスへ再接続
  することが必要 (shittim-chest のクライアントはすでに `upstream_pool` の再接続
  ロジックを備え、再利用可能)。
- **プロセスレベル vs スレッドレベルのフォールトトレランス。** リーダ/フォロワ
  (サブシステム B) は*プロセス/インスタンスレベル*のフォールトトレランスを解決
  するものであり、スレッドレベルではない。タスク内の tokio panic はスーパバイザの
  責務 (タスクの再起動であり、プロセスのフェイルオーバではない) である。B を
  スレッドクラッシュの吸収に使わないこと——重すぎる。
- **明示的にスコープ外。** Raft クォーラム、コンシステントハッシュシャーディング、
  クロスデータセンタ HA —— 「ローリングアップデート + エッジ HA」のスコープにより
  排除される。定常状態が単一インスタンスであるため、レジストリはドレインの協調に
  しか使われない。
## 11. 未解決の問い (保留)
- フェイルオーバ時の worker: 出発点として子プロセスモデルを選択;
  「独立デーモン + attach」モデルは高度として記録。
- B のコールド vs ホットスタンバイ: 出発点としてコールドを選択 (昇格時に JSONL
  から復元); ホット (フォロワがリーダのログを tail) は保留。
- entelecheia の `cosmos` サイドカーと shittim-chest の `pglite-proxy` を
  統一 `Worker` 抽象の下に包むか: yes で合意 (三プロジェクト全体で単一の worker
  抽象へ統一)。
---
*翻訳: 英語の権威ある原文は `docs/en/design/platform/supervision-and-rolling-update.md`
です。他の言語 (zhs/zht/ko/fr/es/ru) は i18n 保留中です。*
