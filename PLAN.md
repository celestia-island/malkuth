# malkuth — 项目状态与计划 (PLAN)

> 刷新于 2026-07-14。服务监督工具包，被 shittim-chest 借用做编排层。

## 1. 项目概述

- **名称**：`malkuth`
- **简介**：可组合服务监督工具包（JSON-RPC over TCP/WS/IPC），可组合为 "process supervision + JSON-RPC bus + multi-tenant sandbox" 形态。
- **远程仓库**：https://github.com/celestia-island/malkuth.git
- **技术栈**：Rust / tokio / jsonrpsee / just
- **类别**：library（service supervision）

## 2. 当前状态

- **当前分支**：`dev`
- **工作区**：有未提交改动（1 项：`src/bin/malkuth.rs`）
- **最近提交时间**：2026-07-12
- **最近提交**：`Merge branch 'master' into dev`（前次是 `🔧 Update the log output format to match the sibling CLIs.`）
- **本地领先 `origin/dev`**：0

## 3. 未提交改动

```
 M src/bin/malkuth.rs
```

## 4. 近期进展

- 统一与 sibling CLI 项目的日志输出格式。
- 切到 Git Bash + celestia-devtools on-demand import。
- 之前 master → dev merge 收尾。

## 5. 后续计划

1. **CLI 收尾**：`src/bin/malkuth.rs` 的未提交改动（log 格式微调）随本轮 PLAN.md 一起提交到 dev。
2. **多租户加固**：shittim-chest 借用 malkuth 做 `JSON-RPC bus`，需验证多 `tenant_id` claim 路径。
3. **IPC 命名管道 + Unix 域套接字可观测性**：与 entelecheia scepter 的 WS bridge 对齐。
4. **独立发布**：当前随 shittim-chest 一起使用；可单独 publish 到 crates.io。

## 6. 跨仓依赖

- shittim-chest 强依赖 malkuth 做编排；plana 提供协议类型。
- 本仓无 `path = "../..."` 的硬编码 patch。

---

## 既有详细计划（存档）

详细 API 文档、`JSON-RPC` 消息示例在 `docs/en/`。本文件只承载"当前态 → 后续计划"两部分。
