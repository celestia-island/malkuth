# الإيقاف السلس والتصريف

## المشكلة

تلتقط أغلب خوادم Rust إشارة `ctrl_c` (SIGINT) فقط. لكن `docker stop`، و
`systemctl restart`، وإنهاء البود في Kubernetes ترسل **SIGTERM** — وهو ما
يتجاوز الإيقاف السلس ويقتل الطلبات الجارية بعد انقضاء فترة السماح.

## `DrainController`

يحتفظ `DrainController` بعلامة تصريف مشتركة ويتيح لأي مهمة الانتظار حتى
تُضبط. وهو مبني على `tokio::sync::Notify` + الذرّيات (atomics).

```rust
use malkuth::{DrainController, ShutdownKind};

let ctrl = DrainController::new();
```

## دلالات الإشارات

يثبّت `SignalExitSource` (الميزة `signals`) معالجات قياسية:

| الإشارة | `ShutdownKind` | يُصرِّف؟ | يخرج؟ |
| --- | --- | --- | --- |
| `SIGINT` / `SIGTERM` | `Graceful` | نعم | نعم |
| `SIGHUP` | `Reload` | لا | لا (يستمر بالخدمة) |
| `SIGQUIT` | `Immediate` | نعم (يتخطّى التصريف) | نعم |

لا تُطلق **إشارة** `SIGHUP` التصريف — لا يُحلّ `wait_for_drain()` عند إعادة
التحميل. استخدم `wait_for_signal()` إن احتجت مراقبة عمليات إعادة التحميل
أيضاً.

## التصريف البرمجي

شغّل التصريف من داخل العملية (مثلاً من استدعاء `Lifecycle.Drain` عبر RPC):

```rust
ctrl.begin_drain(ShutdownKind::Graceful); // ← يستيقظ كل من ينتظر wait_for_drain()
```

## مراقبة حالة التصريف

```rust
// فحص غير حاجب:
if ctrl.is_draining() {
    // ارفض الأعمال الجديدة
}

// أي نوع اشتعل؟
if let Some(kind) = ctrl.kind() {
    println!("shutdown kind: {kind:?}");
}

// انتظار غير متزامن (يُحلّ عند Graceful أو Immediate، وليس عند Reload):
let kind = ctrl.wait_for_drain().await;

// انتظار غير متزامن لأي إشارة (يشمل Reload):
let kind = ctrl.wait_for_signal().await;
```

## استخدام `Supervised`

يُركّب `Supervised` كلاً من `DrainController` + مصدر خروج + خطافات تصريف في
حلقة تقديم واحدة:

```rust
use malkuth::{Supervised, Router};
use malkuth::transport::TcpTransport;
use malkuth::Transport;
use std::sync::Arc;

let supervised = Supervised::new()
    .signals()                          // تثبيت معالج إشارات النظام
    .on_drain(MyDrainHook)              // تشغيل التنظيف أثناء الإيقاف
    .drain_budget(std::time::Duration::from_secs(30));

let ctrl = supervised.drain_controller();
let handler = Arc::new(
    Router::new().lifecycle(ctrl, None)
);

// قدّم JSON-RPC حتى تُطلق إشارة، ثم شغّل خطافات التصريف:
supervised
    .serve_rpc(&TcpTransport, "tcp://0.0.0.0:8080", handler)
    .await?;
```

## `ShutdownKind`

```rust
pub enum ShutdownKind {
    Graceful,   // SIGINT / SIGTERM — صرّف ثم اخرج بـ 0
    Immediate,  // SIGQUIT — تخطَّ التصريف واخرج سريعاً
    Reload,     // SIGHUP — أعد تحميل الإعداد ولا تخرج
}
```
