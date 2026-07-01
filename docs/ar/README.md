# Malkuth
<!-- markdownlint-disable MD033 MD041 MD036 -->
<div align="center">

<img src="../logo.webp" alt="Malkuth" width="200"/>

**أداة قابلة للتركيب للإشراف على الخدمات بلغة Rust — JSON-RPC عبر وسائط نقل قابلة للاستبدال، عمال تحت الإشراف، أقفال تنسيق وانتخاب قائد، بالإضافة إلى واجهة سطر أوامر مراقبة.**

[![License](https://img.shields.io/badge/license-SySL%201.0-blue)](../../LICENSE)
[![Rust](https://img.shields.io/badge/rust-1.85%2B-orange.svg)](https://www.rust-lang.org/)
[![GitHub](https://img.shields.io/badge/github-celestia--island%2Fmalkuth-blue.svg)](https://github.com/celestia-island/malkuth)

</div>
<!-- markdownlint-enable MD033 MD041 MD036 -->

<!-- مُبدّل اللغة متاح في الزاوية السفلية اليمنى -->

> **الإصدار 0.1.0** — كريدت واحدة، **مبنية على tokio**. تغلّف واجهة سطر الأوامر
> *أي* برنامج (حتى لو لا يستخدم المكتبة) بمجموعة بودات ووسيط عكسي لاصق.

يساعد Malkuth البرامج الآلية طويلة التشغيل على إنجاز أربع مهام صعبة:

1. **وسائط نقل قابلة للاستبدال** — JSON-RPC عبر حلقة TCP المحلية، أو
   **WebSocket** بعيدة، أو **IPC** محلية (مقابس يونكس / الأنابيب المسماة عبر
   [`interprocess`](https://crates.io/crates/interprocess)). ترايت `Transport`
   واحد، يُوزَّع حسب مخطط URL.
2. **مبني على tokio وخفيف الإطار** — لا يحتاج مسار JSON-RPC إلى أي إطار HTTP
   (axum اختياري، لفحوصات HTTP فقط).
3. **مرافق اختيارية وقابلة للخطاف** — مصدر الخروج، الفحوصات، خطافات نبضة القلب
   والتصريف هي *ترايتات*. استخدم الافتراضات أو قدّم ما خاصّك. ينظّمها مُنسّق
   `Supervised` جاهز يحتوي كل ما يلزم.
4. **واجهة سطر أوامر للمراقبة** — `malkuth -- <cmd>` يغلّف برنامجاً بمراقبة
   الملفات، ومجموعة بودات، ووسيط عكسي لاصق من الطبقة الرابعة.

## تخطيط مساحة العمل
راجع [README الرئيسي](../../README.md) لمصفوفة الميزات الكاملة واستخدام واجهة
سطر الأوامر، و[التصميم](./design/supervision-and-rolling-update.md) للاطّلاع
على البنية المعمارية.
