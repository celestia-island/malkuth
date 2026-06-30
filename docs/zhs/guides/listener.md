# 监听器交接

## 问题所在

当服务器进程重启时，存在一个没有人监听该端口的窗口期 —— 进入的连接会被丢弃。
对于零停机的滚动更新，新进程必须从旧进程继承监听套接字。

## 解决方案：套接字激活

systemd（或自定义的启动器）保持监听套接字 fd 打开。当进程重启时，新进程继承该 fd，
并可以立即接受连接 —— 内核会在间隔期间将连接排入队列。

Malkuth 的 `acquire_listener` 以**纯 Rust** 实现（无需 `libsystemd`）：

```rust
use malkuth::acquire_listener;

// Prefers systemd socket activation (fd inherited), falls back to a plain bind.
let listener = acquire_listener("0.0.0.0:8080").await?;
```

启用 `socket-activation` 功能：

```toml
[dependencies]
malkuth = { git = "https://github.com/celestia-island/malkuth.git", branch = "dev", features = ["socket-activation"] }
```

## 工作原理

systemd 设置两个环境变量：

| 变量 | 含义 |
| --- | --- |
| `LISTEN_PID` | 应该继承这些 fd 的进程的 PID（必须等于我们的 PID） |
| `LISTEN_FDS` | 传递的 fd 数量（从 fd 3 开始） |

Malkuth 读取这些变量，验证 `LISTEN_PID == our_pid`，取得 fd 3
（`SD_LISTEN_FDS_START`）的所有权，将其设置为非阻塞模式，并包装为
`tokio::net::TcpListener`。

如果这些变量不存在或 PID 不匹配，则回退到 `TcpListener::bind(addr)`。

## systemd 单元示例

```ini
# /etc/systemd/system/myapp.socket
[Socket]
ListenStream=8080

[Install]
WantedBy=sockets.target
```

```ini
# /etc/systemd/system/myapp.service
[Service]
ExecStart=/usr/bin/myapp
# systemd passes the socket fd automatically when the socket unit is active
```

使用此配置，`systemctl restart myapp` 不会丢弃任何进行中的连接：内核会
在新进程启动并继承 fd 期间将它们保留在监听队列中。
