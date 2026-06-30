# クイックスタート

## malkuth をプロジェクトに追加する

```toml
[dependencies]
malkuth = { git = "https://github.com/celestia-island/malkuth.git", branch = "dev" }
# Optional features:
#   socket-activation  — inherit a listener fd from systemd
#   file-lock          — POSIX flock coordination-lock backend
#   lease              — lease-based file lock with TTL auto-expiry
#   replica            — InstanceRegistry trait (load-balancing)
#   leader-follower    — LeaderElector trait (active-passive HA)
```

## グレースフルシャットダウンとプローブを備えた最小サーバー

```rust
use malkuth::{acquire_listener, probe_router, ProbeState, DrainController};

#[tokio::main]
async fn main() -> std::io::Result<()> {
    // 1. Acquire a listener — prefers systemd socket activation,
    //    falls back to binding the given address.
    let listener = acquire_listener("0.0.0.0:8080").await?;

    // 2. Create probe state and install the drain controller.
    let probe = ProbeState::new(env!("CARGO_PKG_VERSION"));
    let ctrl = DrainController::install();

    // 3. Build your router, merging the probe routes.
    let app = axum::Router::new()
        .route("/", axum::routing::get(|| async { "hello" }))
        .merge(probe_router(probe))
        .with_state(());

    // 4. Serve with graceful shutdown: SIGINT/SIGTERM trigger drain,
    //    SIGQUIT forces immediate exit, SIGHUP reloads (keeps serving).
    axum::serve(listener, app)
        .with_graceful_shutdown(async {
            ctrl.wait_for_drain().await;
        })
        .await?;
    Ok(())
}
```

## 利用できる機能

| エンドポイント | 目的 |
| --- | --- |
| `GET /healthz` | ライブネス —「プロセスは生きている」（pid、稼働時間、バージョン） |
| `GET /readyz` | レディネス —「サービス可能」（ドレイン中は 503 を返す） |

| シグナル | 挙動 |
| --- | --- |
| `SIGINT` / `SIGTERM` | グレースフルドレイン（処理中のリクエストを完了してから終了） |
| `SIGHUP` | ホットリロード（終了**しない** — サーバーはサービスを継続） |
| `SIGQUIT` | 即時終了（緊急時のみ） |

## フィーチャーフラグ

| フィーチャー | 有効化される機能 |
| --- | --- |
| `socket-activation` | systemd からリスナー fd を継承（ダウンタイムゼロの再起動） |
| `file-lock` | POSIX `flock` ベースの `CoordinationLock` バックエンド |
| `lease` | クラッシュ時に TTL で自動失効するリースベースのファイルロック |
| `replica` | ロードバランシング用レプリカのための `InstanceRegistry` トレイト |
| `leader-follower` | アクティブ・パッシブ HA 用の `LeaderElector` トレイト |

すべてのフィーチャーはオプトインです。デフォルトビルドには unsafe コードが一切なく、tokio + axum にのみ依存します。
