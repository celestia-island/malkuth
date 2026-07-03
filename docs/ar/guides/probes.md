# فحوصات الصحة

## ترايت `ProbeSink`

يفصل Malkuth **حالة الفحص** عن **طريقة كشفها**. يُعرّف ترايت
[`ProbeSink`](../design/supervision-and-rolling-update.md) استعلامين:

```rust
#[async_trait]
pub trait ProbeSink: Send + Sync {
    async fn ready(&self) -> ReadyStatus;
    async fn health(&self) -> HealthStatus;
}
```

يمكن الاستعلام عن أي نوع يُطبّق `ProbeSink` عبر JSON-RPC أو HTTP.

## `ProbeState` — التنفيذ المدمج

يحتفظ `ProbeState` بمعلومات الإصدار، وعلامة حالة التصريف، وعدّاد الجيل، وقائمة
من فحوصات التبعية:

```rust
use malkuth::{ProbeState, DrainState};

let probe = ProbeState::new(env!("CARGO_PKG_VERSION"));

// سجّل تبعية تؤثر على الجاهزية.
// الإغلاق متزامن — أبقه رخيصاً (اقرأ ذرّية، أو انقُب اتصالاً مُخزَّناً).
probe.add_dependency("database", || { /* أعِد true إن كان سليماً */ true });

// اقلب بت التصريف أثناء الإيقاف:
probe.set_drain_state(DrainState::Draining);

// سجّل جيل النشر (ظاهر في استجابة الحالة):
probe.set_generation(Some(2));
```

## الكشف عبر JSON-RPC (الأساسي)

يُسجّل `Router::lifecycle(ctrl, Some(probe))` الطرق القياسية، مستعلمًا
`ProbeSink` عند كل استدعاء:

```rust
use std::sync::Arc;
use malkuth::{ProbeState, Router, Supervised};
use malkuth::transport::TcpTransport;
use malkuth::Transport;

let supervised = Supervised::new().signals();
let ctrl = supervised.drain_controller();
let probe = Arc::new(ProbeState::new("0.1.0"));

let handler = Arc::new(
    Router::new()
        .lifecycle(ctrl, Some(probe.clone()))
        .route("ping", |_| Box::pin(async { Ok(serde_json::json!("pong")) })),
);

supervised.serve_rpc(&TcpTransport, "tcp://0.0.0.0:8080", handler).await?;
```

### `Lifecycle.Health` ← `HealthStatus`

```json
// الطلب: { "jsonrpc": "2.0", "id": 1, "method": "Lifecycle.Health", "params": {} }
// الاستجابة:
{ "alive": true, "pid": 12345, "uptime_secs": 360, "version": "0.1.0" }
```

### `Lifecycle.Status` ← `ReadyStatus`

```json
// الطلب: { "jsonrpc": "2.0", "id": 2, "method": "Lifecycle.Status", "params": {} }
// الاستجابة:
{
  "ready": true,
  "draining": false,
  "dependencies": [{ "name": "database", "ok": true }],
  "generation": 2
}
```

عندما تكون `draining` بقيمة `true` أو أي تبعية `ok: false`، تكون `ready`
بقيمة `false`.

## الكشف عبر HTTP (اختياري، الميزة `probes`)

لفحوصات HTTP بنمط Kubernetes أو موازنات الأحمال الخارجية التي تتوقع HTTP،
فعِّل الميزة `probes` للحصول على مسارات axum:

```rust
use malkuth::{ProbeState, probe_router};

let probe = ProbeState::new(env!("CARGO_PKG_VERSION"));
probe.add_dependency("database", || { true });

let app = axum::Router::new()
    .merge(probe_router(probe));   // GET /healthz + GET /readyz
```

| النقطة النهائية | تُعيد | حالة HTTP |
| --- | --- | --- |
| `GET /healthz` | `HealthStatus` | دائماً 200 |
| `GET /readyz` | `ReadyStatus` | 200 إن كان جاهزاً، و503 عند التصريف / تبعية متعطّلة |

أشكال الاستجابة مطابقة لطرق JSON-RPC — إذ يُطبّق `ProbeState`
ترايت `ProbeSink`، لذا كلا المسارين يستعلمان نفس الحالة الأساسية.

## ربط التصريف بالفحوصات

أثناء الإيقاف السلس، اضبط حالة التصريف كي تعكسها `Lifecycle.Status` (و
`/readyz`):

```rust
use malkuth::{DrainController, DrainState, ShutdownKind};

let ctrl = DrainController::new();
let probe = ProbeState::new("0.1.0");

tokio::spawn({
    let probe = probe.clone();
    let ctrl = ctrl.clone();
    async move {
        ctrl.wait_for_drain().await;
        probe.set_drain_state(DrainState::Draining);
    }
});
```

الآن يرى المنسّق الجاهزية تنقلب إلى `false` **قبل** خروج العملية — وهذا جوهر
التحديثات المتداولة دون توقف.
