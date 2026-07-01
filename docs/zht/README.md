# Malkuth
<!-- markdownlint-disable MD033 MD041 MD036 -->
<div align="center">

<img src="../logo.webp" alt="Malkuth" width="200"/>

**可組合的 Rust 服務監管工具包 —— 基於可插拔傳輸的 JSON-RPC、受監管的 worker、協調鎖與領導者選舉，外加一個 watchdog 命令列工具。**

[![License](https://img.shields.io/badge/license-SySL%201.0-blue)](../../LICENSE)
[![Rust](https://img.shields.io/badge/rust-1.85%2B-orange.svg)](https://www.rust-lang.org/)
[![GitHub](https://img.shields.io/badge/github-celestia--island%2Fmalkuth-blue.svg)](https://github.com/celestia-island/malkuth)

</div>
<!-- markdownlint-enable MD033 MD041 MD036 -->

<!-- language switcher is available in the bottom-right corner -->

> **版本 0.1.0** —— 單一 crate，**基於 tokio**。該命令列工具用一個 pod 池與一個
> 黏性反向代理封裝*任何*程式（即使該程式不使用本函式庫）。

Malkuth 幫助自動化、長期執行的程式完成四件難事：

1. **可插拔傳輸** —— 基於 JSON-RPC 的本地 TCP 回環、遠端
   **WebSocket**，或本地 **IPC**（透過
   [`interprocess`](https://crates.io/crates/interprocess) 實作的 Unix socket / 具名管道）。只需一個 `Transport`
   trait，依 URL scheme 分派。
2. **基於 tokio、框架輕量** —— JSON-RPC 路徑不需要任何 HTTP 框架
   （axum 為選用，僅用於 HTTP 探針）。
3. **選用、可掛鉤的設施** —— 退出來源、探針、心跳與排空鉤子是
   *trait*。使用預設實作，或提供你自己的實作。一個開箱即用的
   `Supervised` 協調器將它們串接起來。
4. **一個 watchdog 命令列工具** —— `malkuth -- <cmd>` 用檔案監看、一個
   pod 池與一個 L4 黏性反向代理來封裝程式。

## 工作區佈局
完整的功能矩陣與命令列工具用法請參見[根 README](../../README.md)，架構
請參見[設計](./design/supervision-and-rolling-update.md)。
