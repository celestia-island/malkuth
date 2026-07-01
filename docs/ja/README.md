# Malkuth
<!-- markdownlint-disable MD033 MD041 MD036 -->
<div align="center">

<img src="../logo.webp" alt="Malkuth" width="200"/>

**Rust 向けのコンポーザブルなサービス監視ツールキット —— プラグ可能なトランスポート上の JSON-RPC、監視付きワーカー、協調ロックとリーダー選出、そして watchdog CLI。**

[![License](https://img.shields.io/badge/license-SySL%201.0-blue)](../../LICENSE)
[![Rust](https://img.shields.io/badge/rust-1.85%2B-orange.svg)](https://www.rust-lang.org/)
[![GitHub](https://img.shields.io/badge/github-celestia--island%2Fmalkuth-blue.svg)](https://github.com/celestia-island/malkuth)

</div>
<!-- markdownlint-enable MD033 MD041 MD036 -->

<!-- language switcher is available in the bottom-right corner -->

> **バージョン 0.1.0** —— 単一クレート、**tokio ベース**。CLI は *任意の*
> プログラム（ライブラリを使用しないものでも）を、pod プールと
> スティッキーリバースプロキシで包み込みます。

Malkuth は、自動化された長時間実行されるプログラムが四つの難しいことを行えるよう支援します。

1. **プラグ可能なトランスポート** —— ローカル TCP ループバック、リモート
   **WebSocket**、またはローカル **IPC**（[`interprocess`](https://crates.io/crates/interprocess) による
   Unix ソケット / 名前付きパイプ）上の JSON-RPC。単一の `Transport`
   trait を URL スキームでディスパッチします。
2. **tokio ベース、フレームワーク軽量** —— JSON-RPC パスは HTTP フレームワークを必要としません
   （axum はオプションで、HTTP プローブ専用です）。
3. **オプションのフック可能な機能** —— 終了ソース、プローブ、ハートビートとドレインの
   フックは *trait* です。デフォルトを使うか独自のものを提供してください。バッテリー同梱の
   `Supervised` オーケストレータがそれらをまとめて配線します。
4. **watchdog CLI** —— `malkuth -- <cmd>` はプログラムをファイル監視、
   pod プール、L4 スティッキーリバースプロキシで包み込みます。

## ワークスペース構成
完全な機能マトリクスと CLI の使い方については[ルート README](../../README.md) を、
アーキテクチャについては[設計](./design/supervision-and-rolling-update.md) を参照してください。
