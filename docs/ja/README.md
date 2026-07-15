<p align="center"><img src="https://raw.githubusercontent.com/celestia-island/docs.celestia.world/dev/res/logo/malkuth.webp" alt="Malkuth" width="240" /></p>

<h1 align="center">Malkuth</h1>

<p align="center"><strong>Rust 向けのコンポーザブルなサービス監視ツールキット</strong></p>

<div align="center">

[![License: SySL-1.0](https://img.shields.io/badge/License-SySL--1.0-blue.svg)](https://sysl.celestia.world)
[![GitHub](https://img.shields.io/badge/github-celestia--island%2Fmalkuth-blue.svg)](https://github.com/celestia-island/malkuth)
[![Checks](https://img.shields.io/github/actions/workflow/status/celestia-island/malkuth/checks.yml)](https://github.com/celestia-island/malkuth/actions/workflows/checks.yml)
[![Docs](https://img.shields.io/badge/docs-malkuth.docs.celestia.world-blue)](https://malkuth.docs.celestia.world)
[![docs.rs](https://docs.rs/malkuth/badge.svg)](https://docs.rs/malkuth)

</div>

<div align="center">

[English](../en/README.md) · [简体中文](../zhs/README.md) · [繁體中文](../zht/README.md) · **日本語** · [한국어](../ko/README.md) · [Français](../fr/README.md) · [Español](../es/README.md) · [Русский](../ru/README.md) · [العربية](../ar/README.md)

</div>

Malkuth は、自動化された長時間実行されるプログラムが四つの難しいことを行えるよう支援します。

1. **プラグ可能なトランスポート** — ローカル TCP ループバック、リモート
   **WebSocket**、またはローカル **IPC**（[`interprocess`](https://crates.io/crates/interprocess) による
   Unix ソケット / 名前付きパイプ）上の JSON-RPC。単一の `Transport`
   trait を URL スキームでディスパッチします。
2. **監視付きワーカー** — プロセスを起動し、ヘルスを監視し、障害時に再起動し、シャットダウン前に接続をドレインします。
3. **オプションのフック可能な機能** — 終了ソース、プローブ、ハートビートとドレインの
   フックは *trait* です。デフォルト（OS シグナル終了、axum プローブ、監視付きワーカー）を使うか、
   独自のものを提供してください（例：サーバーが受け取るインバンドの「stop」コマンドからドレインをトリガーする）。
   バッテリー同梱の `Supervised` オーケストレータがそれらをまとめて配線します。
4. **watchdog CLI** — `malkuth -- <cmd>` はプログラムをファイル監視、
   pod プール、L4 スティッキーリバースプロキシで包み込みます。

## CLI として使用

```
malkuth [--watch PATH]... [--proxy PUBLIC:LO-HI] [--pod-count N] -- <cmd> [args...]
```

サーバーの並列コピーを 5 つ実行し（各コピーは `PORT` 環境変数で待ち受け → 3001〜3005 を自動割り当て）、
ポート 3000 のスティッキープロキシでフロントします。

```bash
malkuth --watch ./src --watch ./res \
        --proxy 3000:3000-3999 --pod-count 5 \
        -- cargo run
```

プロキシはコンシステントハッシュ法により各**クライアント IP** を固定のバックエンドにルーティングするため、
クライアントは pod が再起動またはスケールダウンするまで同じ pod にアクセスし続けます — これはグレーリリース
/ ローリング再起動の基盤となります。ファイル変更時には、一度に一つの pod をドレインして再起動します。

## ライブラリとして使用

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

シグナルの代わりに独自のロジックでドレインをトリガーしたい場合は、
`malkuth::ExitSource` を実装して `.exit(...)` 経由で渡してください。
Postgres バックエンドの協調機構が必要ですか？ `pg-lock` フィーチャーが
`CoordinationLock` バックエンドを提供します。

## フィーチャーフラグ

| フィーチャー | 有効化する機能 |
| --- | --- |
| `tcp` *(デフォルト)* | ローカル／リモート TCP 上の JSON-RPC（`tokio::net`） |
| `ws` | WebSocket 上の JSON-RPC（`tokio-tungstenite`） |
| `ipc` | ローカル IPC 上の JSON-RPC（`interprocess`） |
| `signals` *(デフォルト)* | デフォルト OS シグナル `ExitSource`（`tokio::signal`） |
| `worker` | 監視付き子プロセスワーカー（`tokio::process`） |
| `probes` | axum `/healthz` + `/readyz` ルーター |
| `file-lock` | POSIX `flock` `CoordinationLock` バックエンド（unix） |
| `lease` | TTL 自動期限切れ付きファイルリース `CoordinationLock`（クラッシュセーフ） |
| `pg-lock` | PostgreSQL `pg_advisory_lock` バックエンド（`tokio-postgres`） |
| `replica` | インメモリ `InstanceRegistry` |
| `leader-follower` | `LeaseLeaderElector`（リースバックエンド上） |
| `schema` | ワイヤ型向け `schemars::JsonSchema` derive |
| `cli` | `malkuth` watchdog バイナリ（pod プール + スティッキープロキシ） |

## 状況

レイヤー 1〜3（ライフサイクル／ドレイン、プローブ、リスナーハンドオフ）および JSON-RPC コア
（コーデック + サーバー／クライアント + tcp/ws/ipc トランスポート）は実装済みで、
エンドツーエンドでテストされています。CLI の pod プール + スティッキープロキシは動作しています
（E2E 検証済み）。3 つの `CoordinationLock` バックエンド（`file-lock`、`lease`、
`pg-lock`）と `leader-follower` `LeaseLeaderElector` もすべて実装されています。
設計については [docs/design/](../en/design/) を参照してください。

## MCP サーバー

`mcp` feature を有効にして malkuth をビルドし、stdio サーバーを実行します——モデルコンテキストプロトコル（Model Context Protocol）経由で監視ツールキットを AI コーディングアシスタントに公開します：

```bash
malkuth mcp
```

サーバーは 2 つのツールを提供します：`malkuth_supervise`（再起動ポリシー + スライディングウィンドウ型レート制限のもとで監視スーパーバイザの下に worker 群を起動し、それらが終了またはタイムアウトするまでブロックした後、最終ステータススナップショットを返す）と `malkuth_probe`（サービス URL に対する HTTP healthz / readyz チェック）。MCP クライアントに組み込むには：

```json
{
  "mcpServers": {
    "malkuth": { "command": "malkuth", "args": ["mcp"] }
  }
}
```

`mcp` feature は `worker` + `schema` を暗黙に含みます。さらに `rmcp` とプローブツール用の `reqwest` クライアントを追加します。

## ライセンス

SySL-1.0（Synthetic Source License）。[LICENSE](https://sysl.celestia.world) を参照してください。
