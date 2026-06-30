# リスナーの引き継ぎ

## 問題点

サーバープロセスが再起動するとき、誰もポートをリッスンしていない期間が存在し ——
着信接続がドロップされます。ダウンタイムゼロのローリングアップデートでは、
新しいプロセスが古いプロセスからリスニングソケットを継承する必要があります。

## 解決策：ソケットアクティベーション

systemd（またはカスタムランチャー）がリスニングソケットの fd を開いたまま保持します。
プロセスが再起動するとき、新しいプロセスが fd を継承し、ただちに接続を受け付けられます ——
カーネルがその隙間の間に接続をキューに入れます。

Malkuth の `acquire_listener` はこれを**純粋な Rust** で実装しています（`libsystemd` 不要）：

```rust
use malkuth::acquire_listener;

// Prefers systemd socket activation (fd inherited), falls back to a plain bind.
let listener = acquire_listener("0.0.0.0:8080").await?;
```

`socket-activation` フィーチャーを有効にします：

```toml
[dependencies]
malkuth = { git = "https://github.com/celestia-island/malkuth.git", branch = "dev", features = ["socket-activation"] }
```

## 仕組み

systemd は2つの環境変数を設定します：

| 変数 | 意味 |
| --- | --- |
| `LISTEN_PID` | fd を継承すべきプロセスの PID（自身の PID と等しい必要があります） |
| `LISTEN_FDS` | 渡される fd の数（fd 3 から開始） |

Malkuth はこれらを読み取り、`LISTEN_PID == our_pid` を検証した上で、fd 3
（`SD_LISTEN_FDS_START`）の所有権を取得し、ノンブロッキングに設定して、
`tokio::net::TcpListener` でラップします。

変数が存在しない場合や PID が一致しない場合は、`TcpListener::bind(addr)` にフォールバックします。

## systemd ユニットの例

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

この設定により、`systemctl restart myapp` は処理中の接続を一切ドロップしません：
新しいプロセスが起動して fd を継承する間、カーネルがそれらをリッスンキューに保持します。
