# 監聽器交接

## 問題所在

當伺服器行程重啟時，存在一個沒有人監聽該連接埠的窗口期 —— 進入的連線會被丟棄。
對於零停機的滾動更新，新行程必須從舊行程繼承監聽套接字。

## 解決方案：套接字啟用

systemd（或自訂的啟動器）保持監聽套接字 fd 開啟。當行程重啟時，新行程繼承該 fd，
並可以立即接受連線 —— 內核會在間隔期間將連線排入佇列。

Malkuth 的 `acquire_listener` 以**純 Rust** 實現（無需 `libsystemd`）：

```rust
use malkuth::acquire_listener;

// Prefers systemd socket activation (fd inherited), falls back to a plain bind.
let listener = acquire_listener("0.0.0.0:8080").await?;
```

啟用 `socket-activation` 功能：

```toml
[dependencies]
malkuth = { git = "https://github.com/celestia-island/malkuth.git", branch = "dev", features = ["socket-activation"] }
```

## 工作原理

systemd 設定兩個環境變數：

| 變數 | 含義 |
| --- | --- |
| `LISTEN_PID` | 應該繼承這些 fd 的行程的 PID（必須等於我們的 PID） |
| `LISTEN_FDS` | 傳遞的 fd 數量（從 fd 3 開始） |

Malkuth 讀取這些變數，驗證 `LISTEN_PID == our_pid`，取得 fd 3
（`SD_LISTEN_FDS_START`）的所有權，將其設定為非阻塞模式，並包裝為
`tokio::net::TcpListener`。

如果這些變數不存在或 PID 不符合，則回退到 `TcpListener::bind(addr)`。

## systemd 單元範例

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

使用此設定，`systemctl restart myapp` 不會丟棄任何進行中的連線：內核會
在新行程啟動並繼承 fd 期間將它們保留在監聽佇列中。
