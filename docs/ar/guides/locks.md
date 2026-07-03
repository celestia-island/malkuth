# أقفال التنسيق

## التجريد

`CoordinationLock` ترايت قابل للاستبدال للاستبعاد المتبادل بين العمليات.
وهو البدائي المُشتَرك لكلا استراتيجيتَي تحمّل الأعطال:

- **النسخة المتماثلة (Replica)** (النظام الفرعي A) — تنسيق الكتابات المتزامنة
  للحالة المشتركة.
- **القائد/التابع (Leader/Follower)** (النظام الفرعي B) — استخدام القفل كعقد
  القائد.

## الترايت

```rust
#[async_trait]
pub trait CoordinationLock: Send + Sync {
    async fn acquire(&self, key: &str, lease: Duration)
        -> Result<Box<dyn LockGuard>, LockError>;
}

#[async_trait]
pub trait LockGuard: Send + Sync {
    async fn release(&mut self);
}
```

## الخلفيات

### `FileLock` (POSIX `flock`) — الميزة `file-lock`

```toml
malkuth = { features = ["file-lock"] }
```

```rust
use malkuth::lock::FileLock;
use malkuth::CoordinationLock;
use std::time::Duration;

let lock = FileLock::new("/var/lib/myapp/locks");
let mut guard = lock.acquire("write-queue", Duration::from_secs(30)).await?;
// ... عمل حصري ...
guard.release().await; // أو فقط ألقِ guard
```

ملف قفل واحد لكل `key`، يُنشَأ تحت الدليل الجذر. يستخدم
`flock(LOCK_EX | LOCK_NB)` لأقفال حصرية غير حاجبة. إن كانت عملية أخرى تملك
القفل، يُعيد `LockError::Contended`.

> **أنظمة Unix فقط.** يستخدم `FileLock` أمر POSIX `flock` ومتاح فقط على أهداف
> Unix (Linux، وmacOS، وBSD).

### `LeaseLock` (عقد ملف مع TTL) — الميزة `lease`

```toml
malkuth = { features = ["lease"] }
```

```rust
use malkuth::lease::LeaseLock;
use malkuth::CoordinationLock;

let lock = LeaseLock::new("/var/lib/myapp/leases");
let mut guard = lock.acquire("device-leader", Duration::from_secs(10)).await?;
// يُجدِّد العقد نفسه في الخلفية تلقائياً. إن تحطّمت العملية،
// ينتهي العقد بعد انقضاء TTL ويمكن لعملية أخرى الاستحواذ عليه.
guard.release().await;
```

يستخدم إعادة تسمية ملف مؤقت ذرّية (CAS) للاستحواذ على العقد. يُجدّده خيطٌ في
الخلفية كل TTL/3. عند التحطّم، يسمح الطابع الزمني `expires_at_ms` لملف العقد
للمستحوزِد التالي بأخذ الملكية.

### `PgLock` (قفل استشاري في PostgreSQL) — الميزة `pg-lock`

```toml
malkuth = { features = ["pg-lock"] }
```

```rust
use malkuth::pg_lock::PgLock;
use malkuth::CoordinationLock;
use tokio_postgres::NoTls;

let (client, connection) = tokio_postgres::connect("host=localhost dbname=myapp", NoTls).await?;
let lock = PgLock::new(std::sync::Arc::new(client));
let mut guard = lock.acquire("shared-config", Duration::from_secs(30)).await?;
guard.release().await;
```

يستخدم `pg_try_advisory_lock` / `pg_advisory_unlock` على مستوى الجلسة بمفتاح
bigint مُشتقّ من سلسلة `key` عبر تجزئة FNV-1a.

## متى تستخدم أيّاً منها

| السيناريو | الخلفية |
| --- | --- |
| مضيف واحد، لن يتحطّم حائز القفل | `FileLock` |
| مضيف واحد، قد يتحطّم حائز القفل | `LeaseLock` |
| عدة مضيفين، Postgres مشترك | `PgLock` |
| عدة مضيفين، لا قاعدة بيانات مشتركة | خارجي (etcd، Consul) |
