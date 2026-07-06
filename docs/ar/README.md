<p align="center"><img src="https://raw.githubusercontent.com/celestia-island/malkuth/master/docs/logo.webp" alt="Malkuth" width="240" /></p>

<h1 align="center">Malkuth</h1>

<p align="center"><strong>أداة قابلة للتركيب للإشراف على الخدمات بلغة Rust</strong></p>

<div align="center">

[![License](https://img.shields.io/badge/license-SySL%201.0-blue)](https://sysl.celestia.world) [![Rust](https://img.shields.io/badge/rust-1.85%2B-orange.svg)](https://www.rust-lang.org/) [![GitHub](https://img.shields.io/badge/github-celestia--island%2Fmalkuth-blue.svg)](https://github.com/celestia-island/malkuth) [![Checks](https://img.shields.io/github/actions/workflow/status/celestia-island/malkuth/checks.yml)](https://github.com/celestia-island/malkuth/actions/workflows/checks.yml) [![Docs](https://img.shields.io/badge/docs-malkuth.docs.celestia.world-blue)](https://malkuth.docs.celestia.world)

</div>

<div align="center">

[English](../en/README.md) · [简体中文](../zhs/README.md) · [繁體中文](../zht/README.md) · [日本語](../ja/README.md) · [한국어](../ko/README.md) · [Français](../fr/README.md) · [Español](../es/README.md) · [Русский](../ru/README.md) · **العربية**

</div>

يساعد Malkuth البرامج الآلية طويلة التشغيل على إنجاز أربع مهام صعبة:

1. **وسائط نقل قابلة للاستبدال** — JSON-RPC عبر حلقة TCP المحلية، أو
   **WebSocket** بعيدة، أو **IPC** محلية (مقابس يونكس / الأنابيب المسماة عبر
   [`interprocess`](https://crates.io/crates/interprocess)). ترايت `Transport`
   واحد، يُوزَّع حسب مخطط URL.
2. **عمال تحت الإشراف** — تشغيل عملية، مراقبة صحتها، إعادة تشغيلها عند الفشل، تصريف الاتصالات قبل الإغلاق.
3. **مرافق اختيارية وقابلة للربط** — مصدر الخروج، الفحوصات، خطافات نبضة القلب
   والتصريف هي *ترايتات*. استخدم الافتراضيات (خروج بإشارة نظام التشغيل، فحوصات
   axum، عمال تحت الإشراف) أو قدّم ما خاصّك (مثلاً تشغيل التصريف من أمر "إيقاف"
   داخلي يستقبله خادمك). ينظّمها مُنسّق `Supervised` جاهز يحتوي كل ما يلزم.
4. **واجهة سطر أوامر للمراقبة** — `malkuth -- <cmd>` يغلّف برنامجاً بمراقبة
   الملفات، ومجموعة بودات، ووسيط عكسي لاصق من الطبقة الرابعة.

## واجهة سطر الأوامر (تغلّف أي شيء)

```
malkuth [--watch PATH]... [--proxy PUBLIC:LO-HI] [--pod-count N] -- <cmd> [args...]
```

شغّل 5 نسخ متوازية من خادمك (كل واحدة تستمع على متغيّر البيئة `PORT` ←
تخصّص لنفسها 3001–3005)، أمامها وسيط عكسي لاصق على المنفذ 3000:

```bash
malkuth --watch ./src --watch ./res \
        --proxy 3000:3000-3999 --pod-count 5 \
        -- cargo run
```

يُوجّه الوسيط كل **عنوان IP للعميل** إلى خادم خلفي ثابت عبر تجزئة متسقة، بحيث
يستمر العميل في ضرب نفس البود حتى يُعاد تشغيله أو يُخفّض عدده — وهذا أساس
الإصدار الرمادي / إعادة التشغيل المتدرّج. عند تغيّر ملف يُصرّف ويعيد تشغيل بود
واحداً تلو الآخر.

## كمكتبة

```toml
[dependencies]
malkuth = "0.1"
# features: tcp (default) | ws | ipc | signals (default) | worker | probes |
#           file-lock | lease | pg-lock | replica | leader-follower | schema | cli
```

```rust
use std::sync::Arc;
use malkuth::{Client, Router, Server, Supervised, Transport};
use malkuth::transport::TcpTransport;
use serde_json::json;

#[tokio::main]
async fn main() -> std::io::Result<()> {
    // Bind once; build a router with the standard lifecycle RPC + your methods.
    let lis = TcpTransport.listen("tcp://127.0.0.1:0").await?;
    let supervised = Supervised::new().signals();           // OS-signal exit source
    let ctrl = supervised.drain_controller();
    let handler = Arc::new(
        Router::new()
            .lifecycle(ctrl, None)                          // Lifecycle.Drain/Status/...
            .route("ping", |_| Box::pin(async { Ok(json!("pong")) })),
    );
    // Race the server against the exit source, then run drain hooks.
    supervised.serve_rpc_listener(lis, handler).await
}
```

هل تحتاج تشغيل التصريف بمنطقك الخاص بدلاً من الإشارات؟ نفّذ
`malkuth::ExitSource` ومرّرها عبر `.exit(...)`. هل تريد تنسيقاً مدعوماً
بـ Postgres؟ ميزة `pg-lock` توفّر خلفية `CoordinationLock`.

## علامات الميزات

| الميزة | تُفعّل |
| --- | --- |
| `tcp` *(افتراضي)* | JSON-RPC عبر TCP محلي/بعيد (`tokio::net`) |
| `ws` | JSON-RPC عبر WebSocket (`tokio-tungstenite`) |
| `ipc` | JSON-RPC عبر IPC محلي (`interprocess`) |
| `signals` *(افتراضي)* | مصدر خروج `ExitSource` افتراضي بإشارات نظام التشغيل (`tokio::signal`) |
| `worker` | عمال عمليات فرعية تحت الإشراف (`tokio::process`) |
| `probes` | موجّه axum `/healthz` + `/readyz` |
| `file-lock` | خلفية `CoordinationLock` عبر POSIX `flock` (unix) |
| `lease` | `CoordinationLock` بعقد إيجار ملف مع انتهاء تلقائي TTL (آمن ضد الأعطال) |
| `pg-lock` | خلفية `pg_advisory_lock` من PostgreSQL (`tokio-postgres`) |
| `replica` | `InstanceRegistry` في الذاكرة |
| `leader-follower` | `LeaseLeaderElector` (فوق خلفية عقد الإيجار) |
| `schema` | اشتقاقات `schemars::JsonSchema` لأنواع البروتوكول |
| `cli` | ثنائي المراقبة `malkuth` (مجموعة بودات + وسيط عكسي لاصق) |

## الحالة

الطبقات 1–3 (دورة الحياة/التصريف، الفحوصات، تسليم المستمع) ونواة JSON-RPC
(المُشفّر + الخادم/العميل + وسائط النقل tcp/ws/ipc) مُنفَّذة ومُختبرة
من الطرف إلى الطرف. مجموعة البودات + الوسيط العكسي اللاصق في واجهة سطر الأوامر
يعملان (مُتحقّق منهما e2e). جميع خلفيات `CoordinationLock` الثلاث
(`file-lock`، `lease`، `pg-lock`) و `LeaseLeaderElector` الخاص بـ `leader-follower`
مُنفَّذة. راجع [docs/design/](../en/design/) للاطّلاع على التصميم.

## الترخيص

[SySL-1.0（Synthetic Source License）](https://sysl.celestia.world)。
