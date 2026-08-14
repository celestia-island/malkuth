//! File watcher: emits the set of changed paths (debounced) when watched paths
//! change. The pod manager consumes it to perform a rolling restart.
//!
//! Debounce semantics: events are accumulated into a set and delivered once,
//! when no further events have arrived for the debounce window (trailing edge).
//! This batches bursty changes (e.g. a whole deployment) into one signal.
//!
//! Watch-arm robustness: `Watcher::watch` can fail transiently for paths on
//! network filesystems (NFS attribute/lookup races at process start, a mount
//! that is momentarily absent). A failed watch used to disable that path
//! silently for the whole service lifetime. Failed paths are now retried on
//! a fixed backoff until the watch arms; on recovery the path is emitted as
//! a synthetic change so the supervisor catches up with anything that
//! happened while the path was unwatched (a no-change build then skips the
//! restart, so the catch-up is cheap when nothing actually changed).

use std::{
    collections::HashSet,
    path::{Path, PathBuf},
    time::{Duration, Instant},
};
use tokio::sync::mpsc;

use notify::{EventKind, RecommendedWatcher, RecursiveMode, Watcher};
use tracing::{debug, info, warn};

/// Default debounce when no explicit value is given.
const DEFAULT_DEBOUNCE: Duration = Duration::from_secs(3);

/// Backoff between watch retries for paths that failed to arm.
const WATCH_RETRY_DELAY: Duration = Duration::from_secs(5);

/// Spawn a watcher over `paths`. Returns a receiver that yields the set of
/// changed paths each time a (debounced) change is observed. Drops cleanly when
/// the receiver is dropped.
pub fn spawn(paths: Vec<PathBuf>, debounce_secs: u64) -> mpsc::Receiver<Vec<PathBuf>> {
    let debounce = if debounce_secs == 0 {
        DEFAULT_DEBOUNCE
    } else {
        Duration::from_secs(debounce_secs)
    };
    let (tx, rx) = mpsc::channel::<Vec<PathBuf>>(16);
    if paths.is_empty() {
        return rx;
    }
    let tx_signal = tx.clone();
    std::thread::spawn(move || {
        let (evt_tx, evt_rx) = std::sync::mpsc::channel::<notify::Result<notify::Event>>();
        let mut watcher = match RecommendedWatcher::new(
            move |res| {
                let _ = evt_tx.send(res);
            },
            notify::Config::default(),
        ) {
            Ok(w) => w,
            Err(e) => {
                warn!(error = %e, "failed to create file watcher");
                return;
            }
        };
        // Paths whose watch failed to arm, with the instant to retry at.
        // A watch root removed at runtime also lands here when its Remove
        // event fires (see the event pass below). Best-effort: inotify gaps
        // like MOVE_SELF renames or NFS unmounts emit no usable event and
        // stay undetected — same as before this retry existed.
        let mut failed: Vec<(PathBuf, Instant)> = Vec::new();
        for p in &paths {
            if !arm_watch(&mut watcher, p) {
                failed.push((p.clone(), Instant::now() + WATCH_RETRY_DELAY));
            }
        }
        // `watcher` itself is kept alive for the thread's lifetime below
        // (the retry pass re-arms through it).
        let mut pending: HashSet<PathBuf> = HashSet::new();
        let mut window_end: Option<Instant> = None;
        loop {
            let now = Instant::now();

            // Fire the debounce window once it has fully elapsed.
            if window_end.is_some_and(|end| end <= now) {
                let changed: Vec<PathBuf> = pending.drain().collect();
                info!(?changed, "file change → schedule restart");
                if tx_signal.blocking_send(changed).is_err() {
                    break; // receiver dropped → stop
                }
                window_end = None;
                continue;
            }

            // Retry due watch re-arms. A success after a failure means the
            // path was unwatched for a while: emit it as a synthetic change
            // so the supervisor catches up (build-with-no-change is cheap).
            if !failed.is_empty() {
                let mut still_failed: Vec<(PathBuf, Instant)> = Vec::with_capacity(failed.len());
                let mut recovered: Vec<PathBuf> = Vec::new();
                for (p, at) in failed.drain(..) {
                    if at > now {
                        still_failed.push((p, at));
                        continue;
                    }
                    if arm_watch(&mut watcher, &p) {
                        recovered.push(p);
                    } else {
                        still_failed.push((p, now + WATCH_RETRY_DELAY));
                    }
                }
                failed = still_failed;
                if !recovered.is_empty() {
                    info!(?recovered, "watch re-armed after failure");
                    for p in recovered {
                        pending.insert(p);
                    }
                    window_end = Some(
                        Instant::now()
                            .checked_add(debounce)
                            .unwrap_or(Instant::now() + DEFAULT_DEBOUNCE),
                    );
                    continue;
                }
            }

            // Sleep until the nearer of the debounce deadline and the next
            // watch retry (blocking indefinitely when neither is pending).
            let next_deadline = [window_end, failed.iter().map(|(_, at)| *at).min()]
                .into_iter()
                .flatten()
                .min();
            let idle = match next_deadline {
                Some(deadline) => {
                    match evt_rx.recv_timeout(deadline.saturating_duration_since(Instant::now())) {
                        Ok(ev) => Some(ev),
                        Err(std::sync::mpsc::RecvTimeoutError::Timeout) => None, // deadline reached
                        Err(std::sync::mpsc::RecvTimeoutError::Disconnected) => break,
                    }
                }
                None => match evt_rx.recv() {
                    Ok(ev) => Some(ev),
                    Err(_) => break, // event channel disconnected
                },
            };
            let Some(Ok(ev)) = idle else { continue };
            if matches!(
                ev.kind,
                EventKind::Create(_) | EventKind::Modify(_) | EventKind::Remove(_)
            ) {
                // A removed watch root invalidates its recursive watch —
                // route it back to the retry list so it re-arms when the
                // path comes back (NFS remount, deploy-time rename, ...).
                for p in &ev.paths {
                    if watched_root_removed(&ev.kind, p, &paths)
                        && !failed.iter().any(|(fp, _)| fp == p)
                    {
                        debug!(path = %p.display(), "watch root removed, will re-arm");
                        failed.push((p.clone(), Instant::now() + WATCH_RETRY_DELAY));
                    }
                }
                for p in ev.paths {
                    pending.insert(p);
                }
                window_end = Some(
                    Instant::now()
                        .checked_add(debounce)
                        .unwrap_or(Instant::now() + DEFAULT_DEBOUNCE),
                );
                debug!(?pending, "file change during debounce window");
            }
        }
    });
    rx
}

/// Try to arm a recursive watch on `path`, logging the outcome. Returns true
/// when the watch is established.
fn arm_watch(watcher: &mut RecommendedWatcher, path: &Path) -> bool {
    match watcher.watch(path, RecursiveMode::Recursive) {
        Ok(()) => {
            info!(path = %path.display(), "watching");
            true
        }
        Err(e) => {
            warn!(
                path = %path.display(),
                error = %e,
                retry = ?WATCH_RETRY_DELAY,
                "failed to watch path (will retry)"
            );
            false
        }
    }
}

/// True when the event says a watched root directory itself was removed.
fn watched_root_removed(kind: &EventKind, p: &Path, roots: &[PathBuf]) -> bool {
    matches!(kind, EventKind::Remove(_)) && roots.iter().any(|r| r == p)
}
