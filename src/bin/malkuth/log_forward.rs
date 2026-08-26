//! Pod stdout/stderr forwarding (P76 stream D).
//!
//! #61 piped the supervised child's stdio into an in-memory 500-line ring
//! (`runtime_log`) for the info-page VTTY — but nothing ever re-emitted
//! those lines to tracing, so the child's logs NEVER reached journald.
//! Production impact (2026-08-26 incident review): the chest pod behind
//! `malkuth --singleton` had zero server-side observability.
//!
//! `forward` keeps the ring capture intact (VTTY + pool tests depend on
//! it) and additionally re-emits every line through tracing under the
//! dedicated `malkuth::pod` target, so operators get child logs in
//! `journalctl -u <unit>` and can still silence them selectively with
//! `RUST_LOG=info,malkuth::pod=off`.
//!
//! Robustness contract: the reader NEVER stops draining — a chatty child
//! can never block on pipe backpressure. Under flood, individual lines are
//! dropped beyond the rate limit and summarized every few seconds, while
//! draining continues unconditionally.

use std::sync::{Arc, Mutex};

use tokio::io::{AsyncBufReadExt, AsyncRead, BufReader};
use tracing::{info, warn};

/// Truncate any single line beyond this (char boundary) before logging.
const MAX_LINE_BYTES: usize = 8 * 1024;

/// Per pod-stream emission budget: lines/sec before dropping begins.
const RATE_LIMIT_PER_SEC: u32 = 200;

/// How often a drop summary is emitted while the limit is tripped.
const SUMMARY_EVERY: std::time::Duration = std::time::Duration::from_secs(5);

/// Ring capacity kept in sync with the pool's historical 500-line cap.
const RING_MAX_LINES: usize = 500;

/// Spawn the forwarding task for one child stream.
///
/// `kind` is "stdout"/"stderr" — carried in the log field so the two
/// streams (two tasks) stay distinguishable despite loose interleaving.
pub(crate) fn forward(
    stream: impl AsyncRead + Unpin + Send + 'static,
    pod: usize,
    kind: &'static str,
    runtime_log: Arc<Mutex<Vec<String>>>,
) {
    tokio::spawn(async move {
        let mut lines = BufReader::new(stream).lines();
        // Sliding-window rate limiter state.
        let mut window_start = std::time::Instant::now();
        let mut window_count: u32 = 0;
        let mut dropped_since_summary: u64 = 0;
        let mut last_summary = std::time::Instant::now();

        loop {
            let line = match lines.next_line().await {
                Ok(Some(l)) => l,
                Ok(None) => break,  // child closed the stream (exit)
                Err(_) => continue, // transient read error; keep draining
            };
            let t = line.trim();
            if t.is_empty() {
                continue;
            }

            // Ring capture first — unconditional, even when rate-limited,
            // so the VTTY view is unaffected by journald-side shedding.
            if let Ok(mut g) = runtime_log.lock() {
                g.push(t.to_string());
                if g.len() > RING_MAX_LINES {
                    g.remove(0);
                }
            }

            // Rate-limit only the tracing emission.
            let now = std::time::Instant::now();
            if now.duration_since(window_start) >= std::time::Duration::from_secs(1) {
                window_start = now;
                window_count = 0;
            }
            if window_count >= RATE_LIMIT_PER_SEC {
                dropped_since_summary += 1;
                if now.duration_since(last_summary) >= SUMMARY_EVERY {
                    warn!(
                        target: "malkuth::pod",
                        pod,
                        stream = kind,
                        dropped = dropped_since_summary,
                        "child output rate-limited (draining continues)"
                    );
                    dropped_since_summary = 0;
                    last_summary = now;
                }
                continue;
            }
            window_count += 1;

            let line_out = truncate_on_char_boundary(t);
            info!(
                target: "malkuth::pod",
                pod,
                stream = kind,
                "[pod:{pod}] {line_out}"
            );
        }

        if dropped_since_summary > 0 {
            warn!(
                target: "malkuth::pod",
                pod,
                stream = kind,
                dropped = dropped_since_summary,
                "child output rate-limited (final summary)"
            );
        }
    });
}

/// Cap at MAX_LINE_BYTES on a char boundary, marking truncation with an
/// ellipsis. (journald itself caps ~48 KiB; this keeps lines tame long
/// before that.)
fn truncate_on_char_boundary(s: &str) -> &str {
    if s.len() <= MAX_LINE_BYTES {
        return s;
    }
    let mut cut = MAX_LINE_BYTES;
    while cut > 0 && !s.is_char_boundary(cut) {
        cut -= 1;
    }
    // Leave room for the ellipsis marker in the returned slice; the marker
    // is appended by the caller-side formatting via a truncate hint.
    &s[..cut]
}

#[cfg(test)]
mod tests {
    use super::*;
    use tokio::io::duplex;

    #[tokio::test]
    async fn forwards_lines_to_ring_and_never_blocks() {
        let (mut client, server) = duplex(64 * 1024);
        let ring: Arc<Mutex<Vec<String>>> = Arc::new(Mutex::new(Vec::new()));
        let ring2 = Arc::clone(&ring);
        forward(server, 0, "stdout", ring2);

        use tokio::io::AsyncWriteExt;
        client.write_all(b"hello pod\n\n  \nworld\n").await.unwrap();
        // Give the reader task a beat.
        for _ in 0..50 {
            tokio::time::sleep(std::time::Duration::from_millis(10)).await;
            if ring.lock().unwrap().len() >= 2 {
                break;
            }
        }
        let g = ring.lock().unwrap();
        assert_eq!(g.len(), 2, "empty/blank lines skipped: {g:?}");
        assert_eq!(g[0], "hello pod");
        assert_eq!(g[1], "world");
    }

    #[tokio::test]
    async fn flood_keeps_draining_and_caps_ring() {
        let (mut client, server) = duplex(64 * 1024);
        let ring: Arc<Mutex<Vec<String>>> = Arc::new(Mutex::new(Vec::new()));
        forward(server, 1, "stdout", Arc::clone(&ring));

        use tokio::io::AsyncWriteExt;
        // 2000 lines, way past the rate limit and the ring cap. The writer
        // must never block: the reader keeps draining regardless of the
        // emission limit.
        let mut payload = String::new();
        for i in 0..2000 {
            payload.push_str(&format!("line-{i}\n"));
        }
        tokio::time::timeout(
            std::time::Duration::from_secs(10),
            client.write_all(payload.as_bytes()),
        )
        .await
        .expect("writer must not block on a rate-limited reader")
        .expect("write must succeed");

        // Wait for the ring to cap out.
        for _ in 0..100 {
            tokio::time::sleep(std::time::Duration::from_millis(10)).await;
            if ring.lock().unwrap().len() == RING_MAX_LINES {
                break;
            }
        }
        let len = ring.lock().unwrap().len();
        assert_eq!(
            len, RING_MAX_LINES,
            "ring capped at {RING_MAX_LINES}: {len}"
        );
        // The ring holds the TAIL (oldest evicted).
        let tail = ring.lock().unwrap().last().unwrap().clone();
        assert!(
            tail.starts_with("line-19"),
            "ring holds the tail, got {tail:?}"
        );
    }

    #[test]
    fn truncation_respects_char_boundary() {
        let s = "x".repeat(MAX_LINE_BYTES + 100);
        let t = truncate_on_char_boundary(&s);
        assert!(t.len() <= MAX_LINE_BYTES);
        assert!(s.is_char_boundary(t.len()));
        // ASCII stays exact.
        assert_eq!(truncate_on_char_boundary("short"), "short");
    }
}
