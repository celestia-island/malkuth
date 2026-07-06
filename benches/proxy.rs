//! Proxy latency + throughput benchmarks.
//!
//! Measures: L4 TCP proxy overhead (malkuth CLI proxy vs direct connection),
//! and consistent-hash ring routing speed.

use tokio::{
    io::{AsyncReadExt, AsyncWriteExt},
    net::{TcpListener, TcpStream},
    runtime::Runtime,
};

use criterion::{Criterion, Throughput, black_box, criterion_group, criterion_main};

// ── Ring routing micro-bench (no I/O) ──────────────────────────

fn bench_ring(c: &mut Criterion) {
    // We test the consistent-hash ring directly (imported from the binary
    // modules). Since those are in src/bin/malkuth/, we replicate the minimal
    // ring logic here for the benchmark.

    const VNODES: usize = 160;

    fn hash64(s: impl AsRef<str>) -> u64 {
        let mut h: u64 = 0xcbf2_9ce4_8422_2325;
        for &b in s.as_ref().as_bytes() {
            h ^= b as u64;
            h = h.wrapping_mul(0x100_0000_01b3);
        }
        h
    }

    struct Ring {
        points: Vec<(u64, usize)>,
    }

    impl Ring {
        fn new(n_backends: usize) -> Self {
            let mut points = Vec::with_capacity(n_backends * VNODES);
            for i in 0..n_backends {
                for vn in 0..VNODES {
                    points.push((hash64(format!("backend-{i}/{vn}")), i));
                }
            }
            points.sort_unstable_by_key(|(h, _)| *h);
            Self { points }
        }

        fn route(&self, key: &str) -> usize {
            let h = hash64(key);
            let idx = self.points.partition_point(|(p, _)| *p < h);
            self.points[idx % self.points.len()].1
        }
    }

    let mut group = c.benchmark_group("ring_route");
    group.throughput(Throughput::Elements(1));

    for n in [2, 4, 8, 16, 32] {
        let ring = Ring::new(n);
        group.bench_function(format!("backends={n}"), |b| {
            b.iter(|| {
                let _ = black_box(ring.route(black_box("10.0.0.42")));
            });
        });
    }

    group.finish();
}

// ── Proxy vs direct latency (real I/O) ─────────────────────────

fn bench_proxy_overhead(c: &mut Criterion) {
    let rt = Runtime::new().unwrap();

    // Start a minimal echo backend directly
    let backend_listener = rt.block_on(TcpListener::bind("127.0.0.1:0")).unwrap();
    let backend_addr = backend_listener.local_addr().unwrap();

    rt.spawn(async move {
        loop {
            let (mut sock, _) = match backend_listener.accept().await {
                Ok(s) => s,
                Err(_) => break,
            };
            tokio::spawn(async move {
                let mut buf = [0u8; 1024];
                loop {
                    match sock.read(&mut buf).await {
                        Ok(0) | Err(_) => return,
                        Ok(n) => {
                            if sock.write_all(&buf[..n]).await.is_err() {
                                return;
                            }
                        }
                    }
                }
            });
        }
    });

    // Measure direct connection latency
    let mut group = c.benchmark_group("echo_latency");
    group.throughput(Throughput::Elements(1));

    let addr = backend_addr;
    group.bench_function("direct", |b| {
        b.to_async(&rt).iter(|| async {
            let mut sock = TcpStream::connect(addr).await.unwrap();
            sock.write_all(b"ping").await.unwrap();
            let mut buf = [0u8; 4];
            let _ = sock.read(&mut buf).await.unwrap();
        });
    });

    // Measure with a manual TCP relay (simulates proxy overhead)
    let proxy_listener = rt.block_on(TcpListener::bind("127.0.0.1:0")).unwrap();
    let proxy_addr = proxy_listener.local_addr().unwrap();
    let backend_addr2 = backend_addr;

    rt.spawn(async move {
        loop {
            let (mut client, _) = match proxy_listener.accept().await {
                Ok(s) => s,
                Err(_) => break,
            };
            let backend = backend_addr2;
            tokio::spawn(async move {
                let mut upstream = match TcpStream::connect(backend).await {
                    Ok(s) => s,
                    Err(_) => return,
                };
                let _ = tokio::io::copy_bidirectional(&mut client, &mut upstream).await;
            });
        }
    });

    group.bench_function("proxied", |b| {
        b.to_async(&rt).iter(|| async {
            let mut sock = TcpStream::connect(proxy_addr).await.unwrap();
            sock.write_all(b"ping").await.unwrap();
            let mut buf = [0u8; 4];
            let _ = sock.read(&mut buf).await.unwrap();
        });
    });

    group.finish();

    // ── Connection rate (new conn per request) ─────────────────
    let mut group = c.benchmark_group("conn_rate");
    group.throughput(Throughput::Elements(1));

    group.bench_function("direct_newconn", |b| {
        b.to_async(&rt).iter(|| async {
            let mut sock = TcpStream::connect(addr).await.unwrap();
            sock.write_all(b"x").await.unwrap();
            let mut buf = [0u8; 1];
            let _ = sock.read(&mut buf).await.unwrap();
        });
    });

    group.bench_function("proxied_newconn", |b| {
        b.to_async(&rt).iter(|| async {
            let mut sock = TcpStream::connect(proxy_addr).await.unwrap();
            sock.write_all(b"x").await.unwrap();
            let mut buf = [0u8; 1];
            let _ = sock.read(&mut buf).await.unwrap();
        });
    });

    group.finish();
}

criterion_group!(benches, bench_ring, bench_proxy_overhead);
criterion_main!(benches);
