# Malkuth
<!-- markdownlint-disable MD033 MD041 MD036 -->
<div align="center">

<img src="../logo.webp" alt="Malkuth" width="200"/>


**長期間稼働するプログラムが自己アップグレードと負荷分散を行うためのインフラストラクチャ**

[![License](https://img.shields.io/badge/license-BSL--1.1-blue.svg)](LICENSE)
[![Rust](https://img.shields.io/badge/rust-1.85%2B-orange.svg)](https://www.rust-lang.org/)
[![GitHub](https://img.shields.io/badge/github-celestia--island%2Fmalkuth-blue.svg)](https://github.com/celestia-island/malkuth)

</div>
<!-- markdownlint-enable MD033 MD041 MD036 -->

**[English](../en/README.md)** &bull; **[简体中文](../zhs/README.md)** &bull;
**[繁體中文](../zht/README.md)** &bull; **[日本語](README.md)** &bull;
**[한국어](../ko/README.md)** &bull; **[Français](../fr/README.md)** &bull;
**[Español](../es/README.md)** &bull; **[Русский](../ru/README.md)**

> **バージョン 0.1.0** — 初期開発段階。独立して完結しており、tokio + axum にのみ依存します。

malkuth は、自動化された長期間稼働するプログラム（デーモン、エージェント、サーバー）が、
次の難しい 2 つを安全に行えるよう支援します。

- **自己アップグレード** — 処理中のジョブや接続を切断することなく、新バージョン
  （または新規にコンパイルしたビルド）を展開します。ダウンタイムゼロのローリングアップデートです。
- **負荷分散** — 処理を分担し状態を調整する複数のインスタンスを実行し、あるインスタンスが
  グレースフルにリタイアしている間に別のインスタンスが引き継げるようにします。

## 構成要素

- **ライフサイクル** — `DrainController` による統一されたシグナルセマンティクス
  （`SIGTERM` / `SIGINT` = ドレイン、`SIGHUP` = リロード、`SIGQUIT` = 即時）。
- **プローブ** — `/healthz`（ライブネス）と `/readyz`（レディネス、ドレインビット付き）を分離し、
  ロードバランサやオーケストレータがノードをルーティング・リタイアできるようにします。
- **ワーカー** — 監視付きの子プロセスリソース。それぞれが障害分離の境界となり、
  OTP 方式の再起動ポリシーとスライディングウィンドウによるレート制限を備えます。
- **リスナーの引き継ぎ** — プレーンバインドへのフォールバックを備えたソケットアクティベーションに
  よるリスナー継承で、ダウンタイムゼロの再起動を実現します。
- **調整ロック** — 同時書き込みの調整やリーダー選出に使う、プラグイン可能な
  `CoordinationLock` トレイト（`file-lock` / `pg-lock` / `lease`）。

## クイックスタート

```toml
[dependencies]
malkuth = { git = "https://github.com/celestia-island/malkuth.git", branch = "dev" }
# features: socket-activation, file-lock, lease, pg-lock, replica, leader-follower
```

```rust
use malkuth::{acquire_listener, probe_router, ProbeState, DrainController};

#[tokio::main]
async fn main() -> std::io::Result<()> {
    // Listener handoff: socket activation, falls back to a plain bind.
    let listener = acquire_listener("0.0.0.0:8080").await?;

    // Probes + signal-aware drain.
    let probe = ProbeState::new(env!("CARGO_PKG_VERSION"));
    let ctrl = DrainController::install();

    let app = axum::Router::new()
        .merge(probe_router(probe)) // GET /healthz, GET /readyz
        .with_state(());

    axum::serve(listener, app)
        .with_graceful_shutdown(async {
            // Resolves on SIGINT / SIGTERM (drain) or SIGQUIT (immediate),
            // but NOT on SIGHUP (reload — the server keeps serving).
            ctrl.wait_for_drain().await;
        })
        .await?;
    Ok(())
}
```

## フィーチャーフラグ

| フィーチャー | 有効化される機能 |
| --- | --- |
| `socket-activation` | リスナー fd の継承（ソケットアクティベーション） |
| `file-lock` | POSIX `flock` による `CoordinationLock` バックエンド |
| `lease` | TTL による自動失効付きのリースベースファイルロック |
| `pg-lock` | PostgreSQL の `pg_advisory_lock` バックエンド（段階的導入） |
| `replica` | `InstanceRegistry` トレイト（負荷分散 / ローリングアップデート） |
| `leader-follower` | `LeaderElector` トレイト（アクティブ・パッシブ HA） |

## ステータス

ライフサイクル + プローブ、監視付きワーカー、リスナーの引き継ぎ、そして `file-lock`
バックエンドを備えた調整ロックトレイトが実装済みです。`replica` / `leader-follower`
の戦略バックエンドはトレイト契約として定義されており、完全な実装は段階的に導入予定です。
設計については [design/](design/) を参照してください。

## ライセンス

Business Source License 1.1（BSL-1.1）。2030-01-01 に Apache-2.0 または MIT の
いずれか（選択可）へ自動的に変換されます。[LICENSE](LICENSE) を参照してください。
