# الإشراف على العمال

## النموذج

**العامل (worker)** هو عملية ابن قابلة للقتل باستقلال تحمل مورداً واحداً بالضبط
(اتصال PLC، أو منفذ تسلسلي، أو عملية جانبية مثل cosmos أو
pglite-proxy). عملية الابن هي **حدّ عزل الأعطال**: إن تحطّم المورد، لا يُعاد
تشغيل شيء سوى العامل — ويستمر الأب في تقديم الخدمة.

## تعريف العمال

```rust
use malkuth::{Supervisor, WorkerSpec, RestartPolicy, DrainController};

let workers = vec![
    WorkerSpec::new("plc-1", "modbus", "/usr/bin/modbus-bridge")
        .args(["--device", "/dev/ttyUSB0"])
        .env("LOG_LEVEL", "debug")
        .policy(RestartPolicy::Permanent),

    WorkerSpec::new("cosmos", "cosmos", "/usr/bin/cosmos-agent")
        .policy(RestartPolicy::Transient), // أعد التشغيل عند الخروج غير الطبيعي فقط
];
```

## سياسات إعادة التشغيل

مقتبسة من Erlang/OTP:

| السياسة | تُعيد التشغيل عند… |
| --- | --- |
| `Permanent` (الافتراضية) | أي خروج، حتى النظيف |
| `Transient` | الخروج غير الطبيعي (غير الصفري) فقط |
| `Temporary` | أبداً |

## تحديد المعدّل

يُطبّق المُشرف **تحديد معدّل بنافذة منزلقة** لمنع عواصف الأعطال:

```rust
let supervisor = Supervisor::new(workers)
    .rate_limit(5, std::time::Duration::from_secs(60)) // 5 إعادات تشغيل كحد أقصى / 60ث
    .cooldown(std::time::Duration::from_secs(30));      // ثم فترة هدوء 30ث
```

إن يتحطّم عامل أكثر من `max_restarts` مرة خلال النافذة، يدخل فترة هدوء قبل
المحاولة التالية.

## تشغيل المُشرف

```rust
use malkuth::DrainController;

let drain = DrainController::new();
let supervisor = Supervisor::new(workers)
    .rate_limit(5, std::time::Duration::from_secs(60));

// شغّل حتى إشارة التصريف:
tokio::spawn(async move {
    let final_status = supervisor.run(drain).await;
    for w in &final_status {
        tracing::info!(worker = %w.id, status = ?w.status, restarts = w.restart_count, "final");
    }
});

// لاحقاً، شغّل الإيقاف:
// drain.begin_drain(ShutdownKind::Graceful);
```

يجري `Supervisor::run` سباقاً بين خروج كل ابن و`wait_for_drain()`. عند
التصريف، تُقتل جميع الأبناء (`kill_on_drop`) وتُعاد لقطات `WorkerInfo`
النهائية.

## لقطات حالة العامل

بعد اكتمال `supervisor.run()`، يُعيد `Vec<WorkerInfo>` بحالة كل عامل النهائية،
وعدد عمليات إعادة التشغيل، وآخر خطأ:

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
