# クイックスタート

## 依存関係の追加

```toml
[dependencies]
malkuth = { git = "https://github.com/celestia-island/malkuth.git", branch = "dev" }
# features: tcp (default) | ws | ipc | signals (default) | worker | probes |
#           file-lock | lease | pg-lock | replica | leader-follower | schema
```

## 最小の JSON-RPC サービス

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

`Supervised` は JSON-RPC サーバーを OS シグナルの終了ソース
（SIGINT/SIGTERM → ドレイン、SIGHUP → リロード、SIGQUIT → 即時終了）と競争させ、
その後、登録されたドレインフックを実行します。`.signals()` を `.exit(your_impl)`
に差し替えると、独自のロジックからドレインをトリガーできます。

## クライアントからの呼び出し

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

## JSON-RPC ライフサイクルプロトコル

`Router::lifecycle(drain, probe)` は 4 つの標準メソッドを登録します：

| メソッド | パラメータ | 結果 | 効果 |
| --- | --- | --- | --- |
| `Lifecycle.Drain` | `{}` | `{ "accepted": true, "draining": true }` | グレースフルドレインを開始 |
| `Lifecycle.Reload` | `{}` | `null` | リロードを開始（終了しない） |
| `Lifecycle.Status` | `{}` | `ReadyStatus` | レディ状態を照会（ドレインビット + 依存関係） |
| `Lifecycle.Health` | `{}` | `HealthStatus` | ライブ状態を照会（pid / アップタイム / バージョン） |

すべてのメッセージは、選択したトランスポート上で NDJSON フレーム化された
JSON-RPC 2.0 で送られます。

## その他のトランスポート

`TcpTransport` を `WsTransport`（feature `ws`、アドレス `ws://host:port`）や
`IpcTransport`（feature `ipc`、アドレス `ipc:/tmp/sock`）に差し替えられます。
あるいは URL スキーム（`tcp://` / `ws://` / `ipc:`）でディスパッチする
`MultiTransport` を使います。

## CLI で任意のプログラムをラップ

```bash
malkuth --watch ./src --proxy 3000:3000-3999 --pod-count 3 -- cargo run
```

これは 3 つのポッドを実行し（`PORT` 環境変数でポート 3001〜3003 を自己割り当て）、
各ポッドがリッスンを開始するまでプローブし、ポート 3000 でスティッキーな
リバースプロキシを前面に置きます（クライアント IP による一貫性ハッシュルーティング）。
`./src` 配下の変更がローリングリスタートをトリガーし、一度に 1 つのポッドずつ
再起動します。
