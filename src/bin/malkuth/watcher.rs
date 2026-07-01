//! File watcher: emits a `Restart` signal (debounced) when any watched path
//! changes. The pod manager consumes it to perform a rolling restart.

use std::path::PathBuf;
use std::sync::Arc;
use std::time::Duration;

use notify::{EventKind, RecommendedWatcher, RecursiveMode, Watcher};
use tokio::sync::mpsc;
use tracing::{info, warn};

/// Debounce window: coalesce a burst of editor saves into one restart.
const DEBOUNCE: Duration = Duration::from_millis(400);

/// Spawn a watcher over `paths`. Returns a receiver that yields `()` each time
/// a (debounced) change is observed. Drops cleanly when the receiver is dropped.
pub fn spawn(paths: Vec<PathBuf>) -> mpsc::Receiver<()> {
    let (tx, rx) = mpsc::channel::<()>(16);
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
        let mut last_fire: Option<std::time::Instant> = None;
        while let Ok(ev) = evt_rx.recv() {
            match ev {
                Ok(e)
                    if matches!(
                        e.kind,
                        EventKind::Create(_) | EventKind::Modify(_) | EventKind::Remove(_)
                    ) =>
                {
                    let now = std::time::Instant::now();
                    let fire = !matches!(last_fire, Some(t) if now.duration_since(t) < DEBOUNCE);
                    if fire {
                        last_fire = Some(now);
                        info!(?e.paths, "file change → schedule restart");
                        if tx_signal.blocking_send(()).is_err() {
                            break; // receiver dropped → stop
                        }
                    }
                }
                _ => {}
            }
        }
    });
    let _ = tx; // keep original tx alive is unnecessary; channel stays open via tx_signal
    let _ = Arc::new(());
    rx
}
