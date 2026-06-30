<!-- markdownlint-disable MD033 MD041 MD036 -->
<div align="center">

<img src="../res/logo.webp" alt="Plana logo" width="200"/>

# Plana

**長時間稼働するプログラムが自身をアップグレードし、負荷を分散するためのインフラストラクチャ**

[![License](https://img.shields.io/badge/license-BSL--1.1-blue.svg)](../LICENSE)
[![Rust](https://img.shields.io/badge/rust-1.85%2B-orange.svg)](https://www.rust-lang.org/)
[![GitHub](https://img.shields.io/badge/github-celestia--island%2Fplana-blue.svg)](https://github.com/celestia-island/plana)

</div>
<!-- markdownlint-enable MD033 MD041 MD036 -->

**[English](../../README.md)** &bull; **[简体中文](../zhs/README.md)** &bull;
**[繁體中文](../zht/README.md)** &bull; **[日本語](../ja/README.md)** &bull;
**[한국어](../ko/README.md)** &bull; **[Français](../fr/README.md)** &bull;
**[Español](../es/README.md)** &bull; **[Русский](../ru/README.md)**

> **バージョン 0.1.0** — 初期開発段階。独立して自己完結しており、
> 依存関係は tokio + axum のみです。

Plana は、自動化された長時間稼働プログラム —— デーモン、エージェント、サーバー —— が
安全にこなすべき難しい二つの課題を支援します。

- **自己アップグレード** —— 処理中のジョブや接続を落とすことなく、新バージョン
  (または新規にコンパイルしたビルド) をロールアウトする、ダウンタイムゼロの
  ローリングアップデート。
- **ロードバランシング** —— 複数のインスタンスを稼働させて作業を分担し状態を協調
  させ、あるインスタンスがグレースフルに退役しながら別のインスタンスが引き継ぐ
  運用。

## 構成要素

- **ライフサイクル** —— `DrainController` による統一的なシグナルセマンティクス
  (`SIGTERM` / `SIGINT` = ドレイン、`SIGHUP` = リロード、`SIGQUIT` = 即時停止)。
- **プローブ** —— `/healthz` (生存) と `/readyz` (準備、ドレインビット付き) を分離し、
  ロードバランサーやオーケストレーターがノードのルーティングや退役を行えるように
  します。
- **ワーカー** —— 監視付きの子プロセスリソース。それぞれが障害分離の境界となり、
  OTP 形式の再起動ポリシーとスライディングウィンドウ方式のレート制限を備えます。
- **リスナー引き継ぎ** —— ソケットアクティベーションによるリスナーの継承と、
  プレーンバインドへのフォールバックにより、ダウンタイムゼロの再起動を実現します。
- **協調ロック** —— プラグ可能な `CoordinationLock` トレイト
  (`file-lock` / `pg-lock` / `lease`) により、並行する書き込みの協調や
  リーダー選出を行います。

## クイックスタート

```toml
[dependencies]
plana = { git = "https://github.com/celestia-island/plana.git", branch = "dev" }
# features: socket-activation, file-lock, lease, pg-lock, replica, leader-follower
```

```rust
use plana::{acquire_listener, probe_router, ProbeState, DrainController};

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
| `socket-activation` | リスナー fd の継承 (ソケットアクティベーション) |
| `file-lock` | POSIX `flock` による `CoordinationLock` バックエンド |
| `lease` | TTL による自動期限切れ付きのリースベースファイルロック |
| `pg-lock` | PostgreSQL `pg_advisory_lock` バックエンド (段階的導入) |
| `replica` | `InstanceRegistry` トレイト (ロードバランシング / ローリングアップデート) |
| `leader-follower` | `LeaderElector` トレイト (アクティブ・パッシブの HA) |

## ステータス

ライフサイクルとプローブ、監視付きワーカー、リスナー引き継ぎ、そして
`file-lock` バックエンドを備えた協調ロックトレイトが実装済みです。
`replica` / `leader-follower` の戦略バックエンドはトレイト契約として定義されており、
完全な実装は段階的に導入される予定です。

## ライセンス

Business Source License 1.1 (BSL-1.1) です。2030-01-01 に Apache-2.0 または MIT の
いずれか自動的に変換されます。詳しくは [LICENSE](../LICENSE) を参照してください。
