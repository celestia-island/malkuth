//! JSON-RPC throughput benchmarks — short-lived, long-lived, and pooled.

use criterion::{BenchmarkId, Criterion, Throughput, black_box, criterion_group, criterion_main};
use std::sync::Arc;
use tokio::runtime::Runtime;

use malkuth::Transport;
use malkuth::codec::take_frame;
use malkuth::transport::TcpTransport;
use malkuth::{Client, ClientPool, Router, Server};
use serde_json::json;

async fn setup_server() -> String {
    let lis = TcpTransport.listen("tcp://127.0.0.1:0").await.unwrap();
    let addr = lis.local_addr().unwrap();
    let dial = format!("tcp://{addr}");
    let handler = Arc::new(Router::new().route("ping", |_| Box::pin(async { Ok(json!("pong")) })));
    tokio::spawn(async move {
        let _ = Server::serve_listener(lis, handler).await;
    });
    tokio::time::sleep(std::time::Duration::from_millis(50)).await;
    dial
}

// ── Short-lived (new conn per call) — baseline ────────────────

fn bench_short_lived(c: &mut Criterion) {
    let rt = Runtime::new().unwrap();
    let dial = rt.block_on(setup_server());

    let mut group = c.benchmark_group("01_short_lived_newconn");
    group.throughput(Throughput::Elements(1));
    group.bench_function("single", |b| {
        b.to_async(&rt).iter(|| {
            let dial = dial.clone();
            async move {
                let mut c = Client::connect(&TcpTransport, &dial).await.unwrap();
                black_box(c.call("ping", json!({})).await.unwrap());
            }
        });
    });
    group.finish();
}

// ── Long-lived (reuse one connection) ─────────────────────────

fn bench_long_lived(c: &mut Criterion) {
    let rt = Runtime::new().unwrap();
    let dial = rt.block_on(setup_server());

    let mut group = c.benchmark_group("02_long_lived_reuse");
    group.throughput(Throughput::Elements(1));

    // One long-lived connection shared across all iterations via Arc<Mutex>.
    let client = Arc::new(tokio::sync::Mutex::new(
        rt.block_on(Client::connect(&TcpTransport, &dial)).unwrap(),
    ));

    group.bench_function("sequential", |b| {
        let client = client.clone();
        b.to_async(&rt).iter(|| {
            let client = client.clone();
            async move {
                let mut c = client.lock().await;
                black_box(c.call("ping", json!({})).await.unwrap());
            }
        });
    });
    group.finish();
}

// ── Pooled (N connections, concurrent calls) ──────────────────

fn bench_pooled(c: &mut Criterion) {
    let rt = Runtime::new().unwrap();
    let dial = rt.block_on(setup_server());

    let mut group = c.benchmark_group("03_pooled_concurrent");

    for &pool_size in &[1, 4, 8, 16] {
        let pool = rt
            .block_on(ClientPool::new(&TcpTransport, &dial, pool_size))
            .unwrap();
        let pool = Arc::new(pool);

        for &concurrency in &[1, 16, 64] {
            group.throughput(Throughput::Elements(concurrency as u64));
            group.bench_with_input(
                BenchmarkId::new(format!("pool{pool_size}"), concurrency),
                &concurrency,
                |b, &conc| {
                    let pool = pool.clone();
                    b.to_async(&rt).iter(|| {
                        let pool = pool.clone();
                        async move {
                            let mut tasks = Vec::with_capacity(conc);
                            for _ in 0..conc {
                                let p = pool.clone();
                                tasks.push(tokio::spawn(async move {
                                    p.call("ping", json!({})).await.unwrap()
                                }));
                            }
                            for t in tasks {
                                black_box(t.await.unwrap());
                            }
                        }
                    });
                },
            );
        }
    }
    group.finish();
}

// ── Codec framing ─────────────────────────────────────────────

fn bench_codec(c: &mut Criterion) {
    let mut group = c.benchmark_group("04_codec_take_frame");

    let small = b"{\"jsonrpc\":\"2.0\",\"id\":1}\n".to_vec();
    group.throughput(Throughput::Bytes(small.len() as u64));
    group.bench_function("30B", |b| {
        b.iter(|| {
            let mut buf = small.clone();
            let _ = black_box(take_frame(&mut buf));
        });
    });

    let large_data = "X".repeat(4000);
    let large =
        format!("{{\"jsonrpc\":\"2.0\",\"id\":1,\"params\":{{\"data\":\"{large_data}\"}}}}\n")
            .into_bytes();
    group.throughput(Throughput::Bytes(large.len() as u64));
    group.bench_function("4KB", |b| {
        b.iter(|| {
            let mut buf = large.clone();
            let _ = black_box(take_frame(&mut buf));
        });
    });

    group.finish();
}

criterion_group!(
    benches,
    bench_short_lived,
    bench_long_lived,
    bench_pooled,
    bench_codec
);
criterion_main!(benches);
