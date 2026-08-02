//! File watcher: emits the set of changed paths (debounced) when watched paths
//! change. The pod manager consumes it to perform a rolling restart.
//!
//! Debounce semantics: events are accumulated into a set and delivered once,
//! when no further events have arrived for the debounce window (trailing edge).
//! This batches bursty changes (e.g. a whole deployment) into one signal.

use std::{
    collections::HashSet,
    path::PathBuf,
    time::{Duration, Instant},
};
use tokio::sync::mpsc;

use notify::{EventKind, RecommendedWatcher, RecursiveMode, Watcher};
use tracing::{debug, info, warn};

/// Default debounce when no explicit value is given.
const DEFAULT_DEBOUNCE: Duration = Duration::from_secs(3);

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
        for p in &paths {
            if let Err(e) = watcher.watch(p, RecursiveMode::Recursive) {
                warn!(path = %p.display(), error = %e, "failed to watch path");
            } else {
                info!(path = %p.display(), "watching");
            }
        }
        // Keep the watcher alive for the thread's lifetime.
        let _keep = watcher;
        let mut pending: HashSet<PathBuf> = HashSet::new();
        let mut window_end: Option<Instant> = None;
        loop {
            let remaining = window_end.map(|end| end.saturating_duration_since(Instant::now()));
            let idle = match remaining {
                Some(Duration::ZERO) => {
                    let changed: Vec<PathBuf> = pending.drain().collect();
                    info!(?changed, "file change → schedule restart");
                    if tx_signal.blocking_send(changed).is_err() {
                        break; // receiver dropped → stop
                    }
                    window_end = None;
                    continue;
                }
                Some(rest) => match evt_rx.recv_timeout(rest) {
                    Ok(ev) => ev,
                    Err(std::sync::mpsc::RecvTimeoutError::Timeout) => continue,
                    Err(std::sync::mpsc::RecvTimeoutError::Disconnected) => break,
                },
                None => match evt_rx.recv() {
                    Ok(ev) => ev,
                    Err(_) => break, // event channel disconnected
                },
            };
            match idle {
                Ok(e)
                    if matches!(
                        e.kind,
                        EventKind::Create(_) | EventKind::Modify(_) | EventKind::Remove(_)
                    ) =>
                {
                    for p in e.paths {
                        pending.insert(p);
                    }
                    window_end = Some(
                        Instant::now()
                            .checked_add(debounce)
                            .unwrap_or(Instant::now() + DEFAULT_DEBOUNCE),
                    );
                    debug!(?pending, "file change during debounce window");
                }
                _ => {}
            }
        }
    });
    rx
}
