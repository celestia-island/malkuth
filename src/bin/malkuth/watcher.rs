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
//! restart, so the catch-up is cheap when nothing actually changed). The
//! synthetic change is suppressed when the path's identity is unchanged —
//! the replacement cases below usually re-arm a watch that another arm
//! already reported, and the identity snapshot dedupes them to one signal.
//!
//! Rename-over robustness: watching a file arms an inode watch, which goes
//! permanently deaf when the file is replaced by a rename-over (the
//! `install(1)` deploy shape): the replacement is a different inode that is
//! never watched, and inotify reports the death of the old inode with
//! MOVE_SELF/DELETE_SELF events that the notify backend does not surface.
//! Two independent backstops close the gap: the parent directory of each
//! watched path is also watched (non-recursively), so the rename surfaces as
//! an event naming the watched path again; and a lightweight identity poll
//! (inode/size/mtime stat every `POLL_INTERVAL`) catches anything neither
//! inotify arm can report (NFS attribute races, MOVE_SELF gaps). Events from
//! the parent arm are only honored when they name a watched path directly —
//! sibling churn in the deploy directory must not restart the service.
//! When an event does name a watched root (Remove, or the MOVE_SELF
//! Modify-Name shape), the root's watch is re-armed so it tracks the inode
//! that now sits at the path. Known limit: nested content under a watched
//! directory root is only covered by the recursive arm — between a
//! directory-swap deploy and the re-arm, deep changes may go unseen until
//! the next signal; the poll stats the root itself, not its subtree.

use std::{
    collections::{HashMap, HashSet},
    path::{Path, PathBuf},
    time::{Duration, Instant, SystemTime},
};
use tokio::sync::mpsc;

use notify::{EventKind, RecommendedWatcher, RecursiveMode, Watcher, event::ModifyKind};
use tracing::{debug, info, warn};

/// Default debounce when no explicit value is given.
const DEFAULT_DEBOUNCE: Duration = Duration::from_secs(3);

/// Backoff between watch retries for paths that failed to arm.
const WATCH_RETRY_DELAY: Duration = Duration::from_secs(5);

/// Interval of the stat-based fallback poll that catches changes inotify
/// cannot see (rename-over replacements, NFS attribute races).
const POLL_INTERVAL: Duration = Duration::from_secs(5);

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
        // Watch arms: the watched paths themselves (recursive, as before) plus
        // each unique parent directory (non-recursive) so rename-over
        // replacements of a watched file are still seen (see module docs).
        let mut watch_list: Vec<(PathBuf, RecursiveMode)> = paths
            .iter()
            .map(|p| (p.clone(), RecursiveMode::Recursive))
            .collect();
        watch_list.extend(
            parent_dirs(&paths)
                .into_iter()
                .map(|p| (p, RecursiveMode::NonRecursive)),
        );
        // Watch arms whose watch failed to arm, with the instant to retry at.
        // A watch root replaced or removed at runtime also lands here when
        // its Remove / Modify-Name event fires (see the event pass below).
        // Best-effort: inotify gaps like silent MOVE_SELF self-renames emit
        // no usable event; those are backstopped by the poll fallback below.
        let mut failed: Vec<(PathBuf, RecursiveMode, Instant)> = Vec::new();
        for (p, mode) in &watch_list {
            if !arm_watch(&mut watcher, p, *mode) {
                failed.push((p.clone(), *mode, Instant::now() + WATCH_RETRY_DELAY));
            }
        }
        // Identity snapshots for the poll fallback, primed immediately so the
        // first poll tick only reports changes from now on.
        let mut identity: HashMap<PathBuf, Option<FileIdentity>> = paths
            .iter()
            .map(|p| (p.clone(), file_identity(p).ok()))
            .collect();
        let mut next_poll = Instant::now() + POLL_INTERVAL;
        // `watcher` itself is kept alive for the thread's lifetime below
        // (the retry pass re-arms through it).
        let mut pending: HashSet<PathBuf> = HashSet::new();
        let mut window_end: Option<Instant> = None;
        loop {
            let now = Instant::now();

            // Fire the debounce window once it has fully elapsed. The window
            // is only ever opened for mapped changes, so an empty fire is a
            // logic bug — skip instead of restarting with nothing.
            if window_end.is_some_and(|end| end <= now) {
                window_end = None;
                if pending.is_empty() {
                    continue;
                }
                let changed: Vec<PathBuf> = pending.drain().collect();
                // Snapshot identities at fire time, not at event time: the
                // deploy's write has settled by the end of the debounce
                // window, so the snapshot reflects the replacement's final
                // inode and the poll backstop stays quiet about it. Only
                // watched paths are tracked; deliveries may also name files
                // under a watched root for the consumer's benefit.
                for w in &changed {
                    if identity.contains_key(w) {
                        identity.insert(w.clone(), file_identity(w).ok());
                    }
                }
                info!(?changed, "file change → schedule restart");
                if tx_signal.blocking_send(changed).is_err() {
                    break; // receiver dropped → stop
                }
                continue;
            }

            // Retry due watch re-arms. A success after a failure means the
            // path was unwatched for a while: emit it as a synthetic change
            // so the supervisor catches up (build-with-no-change is cheap).
            if !failed.is_empty() {
                let mut still_failed: Vec<(PathBuf, RecursiveMode, Instant)> =
                    Vec::with_capacity(failed.len());
                let mut recovered: Vec<PathBuf> = Vec::new();
                for (p, mode, at) in failed.drain(..) {
                    if at > now {
                        still_failed.push((p, mode, at));
                        continue;
                    }
                    if arm_watch(&mut watcher, &p, mode) {
                        recovered.push(p);
                    } else {
                        still_failed.push((p, mode, now + WATCH_RETRY_DELAY));
                    }
                }
                failed = still_failed;
                if !recovered.is_empty() {
                    info!(?recovered, "watch re-armed after failure");
                    // Recovered parent directories map to nothing (their
                    // watched children own the change signal); a recovered
                    // watch root is emitted as a synthetic change only when
                    // its identity changed while it was unwatched — a bare
                    // re-arm usually just re-watches the inode another arm
                    // (parent watch or poll) already reported.
                    let mut changed = false;
                    for w in recovered.iter().flat_map(|p| relevant_paths(p, &paths)) {
                        let id = file_identity(&w).ok();
                        if identity.get(&w) != Some(&id) {
                            changed = true;
                            pending.insert(w.clone());
                            identity.insert(w.clone(), id);
                        }
                    }
                    if !changed {
                        continue;
                    }
                    window_end = Some(
                        Instant::now()
                            .checked_add(debounce)
                            .unwrap_or(Instant::now() + DEFAULT_DEBOUNCE),
                    );
                    continue;
                }
            }

            // Stat-based fallback tick: reports watched paths whose identity
            // (inode/size/mtime) changed without inotify seeing it.
            if now >= next_poll {
                next_poll = now + POLL_INTERVAL;
                let changed = poll_changes(&paths, &mut identity);
                if !changed.is_empty() {
                    info!(?changed, "file change (poll) → schedule restart");
                    for p in changed {
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

            // Sleep until the nearest of the debounce deadline, the next
            // watch retry, and the next poll tick.
            let next_deadline = [
                window_end,
                failed.iter().map(|(_, _, at)| *at).min(),
                Some(next_poll),
            ]
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
                    if watch_needs_rearm(&ev.kind, p, &paths)
                        && !failed.iter().any(|(fp, _, _)| fp == p)
                    {
                        debug!(path = %p.display(), kind = ?ev.kind, "watch root replaced, will re-arm");
                        failed.push((
                            p.clone(),
                            RecursiveMode::Recursive,
                            Instant::now() + WATCH_RETRY_DELAY,
                        ));
                    }
                }
                // Deliver event paths that name a watched change. Events that
                // name none (churn from unrelated siblings in a watched
                // parent directory — logs, temp files, other deploy targets)
                // must be dropped entirely: opening a debounce window for
                // them would couple the service's own restart logging (or
                // any neighbor activity) back into the watcher and loop
                // restarts.
                let mut hit = false;
                for p in &ev.paths {
                    for w in relevant_paths(p, &paths) {
                        hit = true;
                        pending.insert(w);
                    }
                }
                if !hit {
                    continue;
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

/// Try to arm a watch on `path` with `mode`, logging the outcome. Returns true
/// when the watch is established.
fn arm_watch(watcher: &mut RecommendedWatcher, path: &Path, mode: RecursiveMode) -> bool {
    match watcher.watch(path, mode) {
        Ok(()) => {
            info!(path = %path.display(), ?mode, "watching");
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

/// True when the event says a watched root's own inode was removed or
/// replaced, so its watch must be re-armed against whatever now sits at the
/// path. Remove covers deletion; the rename-over and directory-swap deploys
/// kill the old inode with MOVE_SELF, which notify surfaces as
/// `Modify(Name(_))` naming the root rather than as Remove.
fn watch_needs_rearm(kind: &EventKind, p: &Path, roots: &[PathBuf]) -> bool {
    if !roots.iter().any(|r| r == p) {
        return false;
    }
    matches!(
        kind,
        EventKind::Remove(_) | EventKind::Modify(ModifyKind::Name(_))
    )
}

/// Unique parent directories of the watched paths, skipping parents that are
/// themselves watched and empty (bare-filename) parents.
fn parent_dirs(paths: &[PathBuf]) -> Vec<PathBuf> {
    let mut seen = HashSet::new();
    let mut dirs = Vec::new();
    for p in paths {
        if let Some(parent) = p.parent() {
            if parent.as_os_str().is_empty() || paths.iter().any(|r| r == parent) {
                continue;
            }
            if seen.insert(parent.to_path_buf()) {
                dirs.push(parent.to_path_buf());
            }
        }
    }
    dirs
}

/// The event paths that name a real change the supervisor should see: a
/// watched path itself, or any path under a watched (recursive) root. Events
/// are delivered verbatim — the consumer matches changed *files* against the
/// supervised binary path, so a rename-over inside a watched directory must
/// deliver the new file's path, not just the directory root. Sibling churn in
/// a watched parent directory (temp files, logs, other deploy targets) is
/// rejected: opening a debounce window for it would couple the service's own
/// restart logging (or any neighbor activity) back into the watcher and loop
/// restarts.
fn relevant_paths(ev_path: &Path, roots: &[PathBuf]) -> Vec<PathBuf> {
    if roots
        .iter()
        .any(|r| ev_path == r.as_path() || ev_path.starts_with(r.as_path()))
    {
        vec![ev_path.to_path_buf()]
    } else {
        Vec::new()
    }
}

/// Cheap identity of a watched path used by the poll fallback: a rename-over
/// replacement changes the inode, an in-place rewrite changes size/mtime.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
struct FileIdentity {
    inode: u64,
    size: u64,
    modified: Option<SystemTime>,
}

/// Stat `path`, following symlinks so a re-pointed deploy symlink is seen as
/// a change too. `Err` when the path itself is missing (the poll treats an
/// appearing or disappearing file as a change either way).
fn file_identity(path: &Path) -> std::io::Result<FileIdentity> {
    use std::os::unix::fs::MetadataExt;
    let md = std::fs::metadata(path)?;
    Ok(FileIdentity {
        inode: md.ino(),
        size: md.len(),
        modified: md.modified().ok(),
    })
}

/// Stat-based fallback poll: returns the watched paths whose current identity
/// differs from the cached snapshot, updating the cache. `None` identities
/// (missing paths) participate too, so a delete-then-recreate sequence is
/// detected even without inotify.
fn poll_changes(
    roots: &[PathBuf],
    cache: &mut HashMap<PathBuf, Option<FileIdentity>>,
) -> Vec<PathBuf> {
    let mut changed = Vec::new();
    for r in roots {
        let now = file_identity(r).ok();
        if cache.get(r) != Some(&now) {
            changed.push(r.clone());
        }
        cache.insert(r.clone(), now);
    }
    changed
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Unique scratch dir for a test, cleaned up on drop.
    struct TempDir(PathBuf);
    impl TempDir {
        fn new(tag: &str) -> Self {
            let dir = std::env::temp_dir().join(format!(
                "malkuth-watcher-{}-{}",
                tag,
                std::process::id()
            ));
            let _ = std::fs::remove_dir_all(&dir);
            std::fs::create_dir_all(&dir).unwrap();
            TempDir(dir)
        }
        fn path(&self, name: &str) -> PathBuf {
            self.0.join(name)
        }
    }
    impl Drop for TempDir {
        fn drop(&mut self) {
            let _ = std::fs::remove_dir_all(&self.0);
        }
    }

    #[test]
    fn identity_changes_on_in_place_rewrite_and_rename_over() {
        let dir = TempDir::new("identity");
        let file = dir.path("bin");
        std::fs::write(&file, b"v1").unwrap();
        let a = file_identity(&file).unwrap();

        // In-place rewrite: same inode, different content.
        std::fs::write(&file, b"a much longer v2").unwrap();
        let b = file_identity(&file).unwrap();
        assert_eq!(a.inode, b.inode);
        assert_ne!(a, b);

        // Rename-over replacement (the `install(1)` deploy shape): a brand
        // new inode appears at the same path.
        let tmp = dir.path("bin.new");
        std::fs::write(&tmp, b"v3").unwrap();
        std::fs::rename(&tmp, &file).unwrap();
        let c = file_identity(&file).unwrap();
        assert_ne!(b.inode, c.inode);
        assert_ne!(b, c);

        // A missing path is a distinct identity of its own.
        std::fs::remove_file(&file).unwrap();
        assert!(file_identity(&file).is_err());
    }

    #[test]
    fn poll_detects_rename_over_without_inotify() {
        let dir = TempDir::new("poll");
        let file = dir.path("bin");
        std::fs::write(&file, b"v1").unwrap();

        let mut cache: HashMap<PathBuf, Option<FileIdentity>> = HashMap::new();
        // Priming pass reports the path once, then the state is steady.
        assert_eq!(
            poll_changes(std::slice::from_ref(&file), &mut cache),
            vec![file.clone()]
        );
        assert!(poll_changes(std::slice::from_ref(&file), &mut cache).is_empty());

        // Rename-over replacement fires on the next poll tick.
        let tmp = dir.path("bin.new");
        std::fs::write(&tmp, b"v2").unwrap();
        std::fs::rename(&tmp, &file).unwrap();
        assert_eq!(
            poll_changes(std::slice::from_ref(&file), &mut cache),
            vec![file.clone()]
        );
        assert!(poll_changes(std::slice::from_ref(&file), &mut cache).is_empty());

        // Delete-then-recreate is a change both times.
        std::fs::remove_file(&file).unwrap();
        assert_eq!(
            poll_changes(std::slice::from_ref(&file), &mut cache),
            vec![file.clone()]
        );
        std::fs::write(&file, b"v3").unwrap();
        assert_eq!(
            poll_changes(std::slice::from_ref(&file), &mut cache),
            vec![file.clone()]
        );
    }

    #[test]
    fn event_paths_deliver_verbatim_or_drop() {
        let roots = vec![
            PathBuf::from("/srv/app/dist"),
            PathBuf::from("/usr/local/bin/tool"),
        ];
        // Direct hit on a watched path delivers itself.
        assert_eq!(
            relevant_paths(&PathBuf::from("/usr/local/bin/tool"), &roots),
            vec![PathBuf::from("/usr/local/bin/tool")]
        );
        // A child under a watched directory delivers ITSELF, not the root:
        // the consumer matches changed files against the supervised binary
        // path, so a rename-over inside a watched dir must keep its name.
        assert_eq!(
            relevant_paths(&PathBuf::from("/srv/app/dist/assets/x.js"), &roots),
            vec![PathBuf::from("/srv/app/dist/assets/x.js")]
        );
        // Temp/sibling names in a watched file's parent dir are dropped:
        // delivering them would restart the service on any sibling churn in
        // the deploy directory (restart storm). The replacement is caught
        // when it lands under the watched name, or by the poll fallback.
        assert!(relevant_paths(&PathBuf::from("/usr/local/bin/tool.new"), &roots).is_empty());
        // Unrelated paths are dropped.
        assert!(relevant_paths(&PathBuf::from("/opt/other/x"), &roots).is_empty());
        // Component boundary: dist2 is not under dist.
        assert!(relevant_paths(&PathBuf::from("/srv/app/dist2"), &roots).is_empty());
    }

    #[test]
    fn watch_root_removal_and_move_self_rearm() {
        let roots = vec![PathBuf::from("/usr/local/bin/tool")];
        let root = Path::new("/usr/local/bin/tool");
        // Deletion of the root re-arms.
        assert!(watch_needs_rearm(
            &EventKind::Remove(notify::event::RemoveKind::File),
            root,
            &roots
        ));
        // MOVE_SELF (rename-over, directory swap) surfaces as Modify(Name)
        // naming the root and must re-arm too — the old inode's watch would
        // otherwise go deaf against the replacement.
        assert!(watch_needs_rearm(
            &EventKind::Modify(ModifyKind::Name(notify::event::RenameMode::Any)),
            root,
            &roots
        ));
        // Ordinary content/metadata events on the root do not re-arm.
        assert!(!watch_needs_rearm(
            &EventKind::Modify(ModifyKind::Data(notify::event::DataChange::Any)),
            root,
            &roots
        ));
        // The same events for a non-watched path never re-arm.
        let sibling = Path::new("/usr/local/bin/tool.new");
        assert!(!watch_needs_rearm(
            &EventKind::Modify(ModifyKind::Name(notify::event::RenameMode::Any)),
            sibling,
            &roots
        ));
        assert!(!watch_needs_rearm(
            &EventKind::Remove(notify::event::RemoveKind::File),
            sibling,
            &roots
        ));
    }

    #[test]
    fn parent_dirs_are_unique_and_skip_watched_roots() {
        let paths = vec![
            PathBuf::from("/srv/app/bin/tool"),
            PathBuf::from("/srv/app/bin/tool2"),
            PathBuf::from("/srv/app"),
            PathBuf::from("tool3"),
        ];
        // Parents include the parent of a watched directory itself (a
        // rename-over of /srv/app is seen on /srv), stay unique, and skip
        // watched roots and bare-filename paths.
        assert_eq!(
            parent_dirs(&paths),
            vec![PathBuf::from("/srv/app/bin"), PathBuf::from("/srv")]
        );
    }
}
