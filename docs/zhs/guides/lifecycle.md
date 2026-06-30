# 优雅关闭与排空

## 问题所在

大多数 Rust 服务器只捕获 `ctrl_c`（SIGINT）。但 `docker stop`、`systemctl restart`
和 Kubernetes Pod 终止会发送 **SIGTERM** —— 这会绕过你的优雅关闭逻辑，
直接杀掉进行中的请求。

## 解决方案：`DrainController`

`DrainController::install()` 遵循 nginx/Go 的惯例，设置标准信号处理程序：

| 信号 | 含义 | 是否排空？ |
| --- | --- | --- |
| `SIGINT` / `SIGTERM` | 优雅关闭 | 是 |
| `SIGHUP` | 热配置重载 | 否（服务器继续服务） |
| `SIGQUIT` | 立即退出 | 是（跳过排空） |

## 用法

```rust
use malkuth::DrainController;

let ctrl = DrainController::install();

// Pass clones to whoever needs to observe drain:
// - the serve loop (to stop accepting)
// - the probe layer (to set the /readyz draining bit)
// - background tasks (to wind down)

// Block until a drain/immediate signal fires.
let kind = ctrl.wait_for_drain().await;
```

## 接入 `axum::serve`

```rust
axum::serve(listener, app)
    .with_graceful_shutdown(async {
        ctrl.wait_for_drain().await;
    })
    .await?;
```

`wait_for_drain` 会在 `SIGINT`/`SIGTERM`/`SIGQUIT` 时完成，但**不会**在
`SIGHUP` 时完成，因此重载不会意外关闭服务器。

## 观察排空状态

```rust
// Non-blocking check:
if ctrl.is_draining() {
    // refuse new work
}

// Sleep, but wake early if drain begins:
ctrl.sleep_or_drain(std::time::Duration::from_secs(30)).await;
```

## 编程式排空

你也可以从进程内部触发排空（例如管理 RPC）：

```rust
ctrl.begin_drain(malkuth::ShutdownKind::Graceful);
```

## `ShutdownKind`

```rust
pub enum ShutdownKind {
    Graceful,   // SIGINT / SIGTERM — drain, then exit 0
    Immediate,  // SIGQUIT — skip drain, exit fast
    Reload,     // SIGHUP — reload config, do NOT exit
}
```
