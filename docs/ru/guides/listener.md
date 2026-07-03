# Передача listener'а

## Проблема

Когда серверный процесс перезапускается, возникает окно, в течение которого никто не слушает
порт — входящие соединения отбрасываются. Для обновлений без простоя (rolling updates)
новый процесс должен унаследовать слушающий сокет от старого.

## Решение: socket activation

systemd (или пользовательский лаунчер) удерживает fd слушающего сокета открытым. Когда
процесс перезапускается, новый процесс наследует fd и может немедленно принимать соединения —
ядро помещает их в очередь на время разрыва.

`acquire_listener` Malkuth реализует это на **чистом Rust** (без `libsystemd`):

```rust
use malkuth::acquire_listener;

// Prefers systemd socket activation (fd inherited), falls back to a plain bind.
let listener = acquire_listener("0.0.0.0:8080").await?;
```

Включите компонент `socket-activation`:

```toml
[dependencies]
malkuth = { git = "https://github.com/celestia-island/malkuth.git", branch = "dev", features = ["socket-activation"] }
```

## Как это работает

systemd устанавливает две переменные окружения:

| Переменная | Значение |
| --- | --- |
| `LISTEN_PID` | PID процесса, который должен унаследовать fd (должен совпадать с нашим) |
| `LISTEN_FDS` | Количество передаваемых fd (начиная с fd 3) |

Malkuth считывает их, проверяет, что `LISTEN_PID == our_pid`, забирает владение fd 3
(`SD_LISTEN_FDS_START`), переводит его в неблокирующий режим и оборачивает в
`tokio::net::TcpListener`.

Если переменные отсутствуют или PID не совпадает, выполняется откат к
`TcpListener::bind(addr)`.

## Пример systemd-юнита

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

При такой конфигурации `systemctl restart myapp` не отбрасывает ни одного активного
соединения: ядро удерживает их в очереди прослушивания, пока новый процесс стартует
и наследует fd.
