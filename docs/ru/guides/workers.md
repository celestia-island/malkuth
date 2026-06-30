# Супервизия воркеров

## Модель

**Воркер** — это независимо завершаемый дочерний процесс, который владеет ровно одним
ресурсом (подключением к PLC, последовательным портом, sidecar'ом вроде cosmos или pglite-proxy).
Дочерний процесс является **границей изоляции сбоев**: если ресурс падает,
перезапускается только воркер — родитель продолжает обслуживать.

## Определение воркеров

```rust
use malkuth::{Supervisor, WorkerSpec};
use malkuth::RestartPolicy;

let workers = vec![
    WorkerSpec::new("plc-1", "modbus", "/usr/bin/modbus-bridge")
        .args(["--device", "/dev/ttyUSB0"])
        .policy(RestartPolicy::Permanent),

    WorkerSpec::new("cosmos", "cosmos", "/usr/bin/cosmos-agent")
        .policy(RestartPolicy::Transient), // restart only on abnormal exit
];
```

## Политики перезапуска

Заимствованы из Erlang/OTP:

| Политика | Перезапуск при… |
| --- | --- |
| `Permanent` (по умолчанию) | Любом выходе, даже корректном |
| `Transient` | Только аномальном (ненулевом) выходе |
| `Temporary` | Никогда |

## Ограничение частоты

Супервизор применяет **ограничение частоты со скользящим окном** для защиты от шторма сбоев:

```rust
let supervisor = Supervisor::new(workers)
    .rate_limit(5, std::time::Duration::from_secs(60)) // max 5 restarts / 60s
    .cooldown(std::time::Duration::from_secs(30));      // then cooldown 30s
```

Если воркер падает более `max_restarts` раз в пределах окна, он переходит в
период охлаждения перед следующей попыткой.

## Запуск супервизора

```rust
use tokio::sync::watch;

let (shutdown_tx, shutdown_rx) = watch::channel(false);

let supervisor = Supervisor::new(workers)
    .rate_limit(5, std::time::Duration::from_secs(60));

// Run until shutdown signal:
tokio::spawn(async move {
    let final_status = supervisor.run(shutdown_rx).await;
    for w in &final_status {
        tracing::info!(worker = %w.id, status = ?w.status, restarts = w.restart_count, "final");
    }
});

// Later, trigger shutdown:
let _ = shutdown_tx.send(true);
```

## Снимки состояния воркеров

После завершения `supervisor.run()` (при остановке) возвращается `Vec<WorkerInfo>`
с финальным состоянием, количеством перезапусков и последней ошибкой каждого воркера —
полезно для логирования или передачи в систему мониторинга.
