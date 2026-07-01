# Malkuth
<!-- markdownlint-disable MD033 MD041 MD036 -->
<div align="center">

<img src="../logo.webp" alt="Malkuth" width="200"/>

**可组合的 Rust 服务监管工具包 —— 基于可插拔传输的 JSON-RPC、受监管的 worker、协调锁与领导者选举，外加一个 watchdog 命令行工具。**

[![License](https://img.shields.io/badge/license-SySL%201.0-blue)](../../LICENSE)
[![Rust](https://img.shields.io/badge/rust-1.85%2B-orange.svg)](https://www.rust-lang.org/)
[![GitHub](https://img.shields.io/badge/github-celestia--island%2Fmalkuth-blue.svg)](https://github.com/celestia-island/malkuth)

</div>
<!-- markdownlint-enable MD033 MD041 MD036 -->

<!-- language switcher is available in the bottom-right corner -->

> **版本 0.1.0** —— 单个 crate，**基于 tokio**。该命令行工具用一个 pod 池与一个
> 粘性反向代理封装*任何*程序（即使该程序不使用本库）。

Malkuth 帮助自动化、长期运行的程序完成四件难事：

1. **可插拔传输** —— 基于 JSON-RPC 的本地 TCP 回环、远程
   **WebSocket**，或本地 **IPC**（通过
   [`interprocess`](https://crates.io/crates/interprocess) 实现的 Unix 套接字 / 命名管道）。只需一个 `Transport`
   trait，按 URL scheme 分发。
2. **基于 tokio、框架轻量** —— JSON-RPC 路径无需任何 HTTP 框架
   （axum 是可选的，仅用于 HTTP 探针）。
3. **可选、可挂钩的设施** —— 退出源、探针、心跳与排空钩子是
   *trait*。使用默认实现，或提供你自己的实现。一个开箱即用的
   `Supervised` 编排器将它们串联起来。
4. **一个 watchdog 命令行工具** —— `malkuth -- <cmd>` 用文件监视、一个
   pod 池与一个 L4 粘性反向代理来封装程序。

## 工作区布局
完整的功能矩阵与命令行工具用法请参见[根 README](../../README.md)，架构
请参见[设计](./design/supervision-and-rolling-update.md)。
