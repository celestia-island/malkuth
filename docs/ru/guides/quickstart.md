# Быстрый старт

## Добавить зависимость

```toml
[dependencies]
malkuth = { git = "https://github.com/celestia-island/malkuth.git", branch = "dev" }
# features: tcp (default) | ws | ipc | signals (default) | worker | probes |
#           file-lock | lease | pg-lock | replica | leader-follower | schema
```

## Минимальный сервис JSON-RPC

```rust
use std::sync::Arc;
use malkuth::{Router, Supervised};
use malkuth::transport::TcpTransport;
use malkuth::Transport;
use serde_json::json;

#[tokio::main]
async fn main() -> std::io::Result<()> {
    let lis = TcpTransport.listen("tcp://127.0.0.1:0").await?;
    let supervised = Supervised::new().signals();
    let ctrl = supervised.drain_controller();

    let handler = Arc::new(
        Router::new()
            .lifecycle(ctrl, None)            // registers Lifecycle.Drain / Status / Health / Reload
            .route("ping", |_| Box::pin(async { Ok(json!("pong")) })),
    );

    supervised.serve_rpc_listener(lis, handler).await
}
```

`Supervised` состязает JSON-RPC-сервер с источником выхода по сигналу ОС
(SIGINT/SIGTERM → дрейн, SIGHUP → перезагрузка, SIGQUIT → немедленный выход),
после чего выполняет зарегистрированные дрейн-хуки. Замените `.signals()` на
`.exit(your_impl)`, чтобы запускать дрейн из собственной логики.

## Вызов с клиента

```rust
use malkuth::Client;
use malkuth::transport::TcpTransport;
use malkuth::Transport;
use serde_json::json;

let mut c = Client::connect(&TcpTransport, "tcp://127.0.0.1:8080").await?;

// Custom method:
let r = c.call("ping", json!({})).await?;       // → "pong"

// Standard lifecycle methods (registered by Router::lifecycle):
c.notify("Lifecycle.Drain", json!({})).await?;  // → server begins graceful drain
let health = c.call("Lifecycle.Health", json!({})).await?;
// → { "alive": true, "pid": 12345, "uptime_secs": 360, "version": "0.1.0" }
let status = c.call("Lifecycle.Status", json!({})).await?;
// → { "ready": true, "draining": false, "dependencies": [], "generation": null }
```

## Протокол жизненного цикла JSON-RPC

`Router::lifecycle(drain, probe)` регистрирует четыре стандартных метода:

| Метод | Параметры | Результат | Эффект |
| --- | --- | --- | --- |
| `Lifecycle.Drain` | `{}` | `{ "accepted": true, "draining": true }` | Запустить мягкий дрейн |
| `Lifecycle.Reload` | `{}` | `null` | Запустить перезагрузку (без выхода) |
| `Lifecycle.Status` | `{}` | `ReadyStatus` | Запрос готовности (бит дрейна + зависимости) |
| `Lifecycle.Health` | `{}` | `HealthStatus` | Запрос живости (pid / uptime / версия) |

Все сообщения — это JSON-RPC 2.0 в кадрировании NDJSON поверх выбранного
транспорта.

## Другие транспорты

Замените `TcpTransport` на `WsTransport` (feature `ws`, адрес `ws://host:port`)
или `IpcTransport` (feature `ipc`, адрес `ipc:/tmp/sock`). Либо используйте
`MultiTransport`, который диспетчеризует по схеме URL
(`tcp://` / `ws://` / `ipc:`).

## Обёртка для любой программы через CLI

```bash
malkuth --watch ./src --proxy 3000:3000-3999 --pod-count 3 -- cargo run
```

Это запускает 3 пода (самоназначая порты 3001–3003 через переменную окружения
`PORT`), опрашивает каждый, пока не начнёт слушать, и выводит их на липкий
обратный прокси на порту 3000 (маршрутизация по консистентному хешу клиентского
IP). Изменение под `./src` запускает плавающий перезапуск, по одному поду за раз.
