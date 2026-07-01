# البداية السريعة

## إضافة التبعية

```toml
[dependencies]
malkuth = { git = "https://github.com/celestia-island/malkuth.git", branch = "dev" }
# features: tcp (default) | ws | ipc | signals (default) | worker | probes |
#           file-lock | lease | pg-lock | replica | leader-follower | schema
```

## خدمة JSON-RPC مصغّرة

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
            .lifecycle(ctrl, None)            // يسجّل Lifecycle.Drain / Status / Health / Reload
            .route("ping", |_| Box::pin(async { Ok(json!("pong")) })),
    );

    supervised.serve_rpc_listener(lis, handler).await
}
```

يجري `Supervised` سباقاً بين خادم JSON-RPC ومصدر الخروج عبر إشارات النظام
(SIGINT/SIGTERM ← تصريف، SIGHUP ← إعادة تحميل، SIGQUIT ← خروج فوري)، ثم يشغّل
أي خطافات تصريف مُسجَّلة. استبدل `.signals()` بـ `.exit(your_impl)` لتشغيل
التصريف من منطقك الخاص.

## استدعاؤها من عميل

```rust
use malkuth::Client;
use malkuth::transport::TcpTransport;
use malkuth::Transport;
use serde_json::json;

let mut c = Client::connect(&TcpTransport, "tcp://127.0.0.1:8080").await?;

// طريقة مخصّصة:
let r = c.call("ping", json!({})).await?;       // ← "pong"

// طرق دورة الحياة القياسية (التي يسجّلها Router::lifecycle):
c.notify("Lifecycle.Drain", json!({})).await?;  // ← يبدأ الخادم التصريف السلس
let health = c.call("Lifecycle.Health", json!({})).await?;
// ← { "alive": true, "pid": 12345, "uptime_secs": 360, "version": "0.1.0" }
let status = c.call("Lifecycle.Status", json!({})).await?;
// ← { "ready": true, "draining": false, "dependencies": [], "generation": null }
```

## بروتوكول دورة حياة JSON-RPC

يُسجّل `Router::lifecycle(drain, probe)` أربع طرق قياسية:

| الطريقة | المعاملات | النتيجة | الأثر |
| --- | --- | --- | --- |
| `Lifecycle.Drain` | `{}` | `{ "accepted": true, "draining": true }` | بدء التصريف السلس |
| `Lifecycle.Reload` | `{}` | `null` | بدء إعادة التحميل (بدون خروج) |
| `Lifecycle.Status` | `{}` | `ReadyStatus` | الاستعلام عن الجاهزية (بت التصريف + التبعيات) |
| `Lifecycle.Health` | `{}` | `HealthStatus` | الاستعلام عن الحيوية (pid / وقت التشغيل / الإصدار) |

جميع الرسائل هي JSON-RPC 2.0 مُؤطَّرة بصيغة NDJSON عبر وسيط النقل المختار.

## وسائط النقل الأخرى

استبدل `TcpTransport` بـ `WsTransport` (الميزة `ws`، العنوان `ws://host:port`)
أو `IpcTransport` (الميزة `ipc`، العنوان `ipc:/tmp/sock`). أو استخدم
`MultiTransport`، الذي يُوزِّع حسب مخطط URL (`tcp://` / `ws://` / `ipc:`).

## تغليف أي برنامج عبر واجهة سطر الأوامر

```bash
malkuth --watch ./src --proxy 3000:3000-3999 --pod-count 3 -- cargo run
```

يشغّل هذا 3 بودات (تُخصِّص لنفسها المنافذ 3001–3003 عبر متغير البيئة `PORT`)،
وتفحص كل بود حتى تبدأ الاستماع، وتضع أمامها وسيطاً عكسياً لاصقاً على المنفذ
3000 (توجيه بالتجزئة المتّسقة حسب IP العميل). أي تغيير تحت `./src` يُطلق
إعادة تشغيل متداولة، بودة واحدة في كل مرة.
