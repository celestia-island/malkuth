# Супервизия воркеров

## Модель

**Воркер** — это независимо убиваемый дочерний процесс, который владеет ровно
одним ресурсом (соединением с ПЛК, последовательным портом, сайдкаром вроде
cosmos или pglite-proxy). Дочерний процесс — это **граница изоляции сбоев**: если
ресурс падает, перезапускается только воркер — родитель продолжает обслуживать.

## Определение воркеров

```rust
use malkuth::{Supervisor, WorkerSpec, RestartPolicy, DrainController};

let workers = vec![
    WorkerSpec::new("plc-1", "modbus", "/usr/bin/modbus-bridge")
        .args(["--device", "/dev/ttyUSB0"])
        .env("LOG_LEVEL", "debug")
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

Супервизор применяет **ограничение частоты со скользящим окном**, чтобы
предотвратить штормы падений:

```rust
let supervisor = Supervisor::new(workers)
    .rate_limit(5, std::time::Duration::from_secs(60)) // max 5 restarts / 60s
    .cooldown(std::time::Duration::from_secs(30));      // then cooldown 30s
```

Если воркер падает более `max_restarts` раз в пределах окна, перед следующей
попыткой он входит в период охлаждения.

## Запуск супервизора

```rust
use malkuth::DrainController;

let drain = DrainController::new();
let supervisor = Supervisor::new(workers)
    .rate_limit(5, std::time::Duration::from_secs(60));

// Run until drain signal:
tokio::spawn(async move {
    let final_status = supervisor.run(drain).await;
    for w in &final_status {
        tracing::info!(worker = %w.id, status = ?w.status, restarts = w.restart_count, "final");
    }
});

// Later, trigger shutdown:
// drain.begin_drain(ShutdownKind::Graceful);
```

`Supervisor::run` состязает выход каждого дочернего процесса с
`wait_for_drain()`. При дрейне все дочерние процессы убиваются (`kill_on_drop`),
и возвращаются финальные снимки `WorkerInfo`.

## Снимки состояния воркеров

После завершения `supervisor.run()` возвращается `Vec<WorkerInfo>` с финальным
состоянием каждого воркера, числом перезапусков и последней ошибкой:

```rust
pub struct WorkerInfo {
    pub id: String,
    pub kind: String,
    pub status: WorkerStatus,     // Starting | Running | Stopped | Failed
    pub restart_policy: RestartPolicy,
    pub restart_count: u32,
    pub last_error: Option<String>,
}
```
