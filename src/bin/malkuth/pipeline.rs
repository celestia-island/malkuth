//! Multi-stage build pipeline: the upgrade path from the single `--build`
//! command to composable deploy units.
//!
//! A trigger (a debounced file change from the watcher, or an upstream ref
//! movement from `--watch-remote`) runs the pipeline:
//!
//! 1. named stages (`--build-stage NAME=CMD`, sequential, fail-stop) — or the
//!    legacy single `--build` command as one anonymous stage;
//! 2. the privileged install hook (`--install CMD`) after every stage
//!    succeeded — typically a sudoers-narrowed root helper that copies the
//!    artifact into the deploy path and restarts nothing itself;
//! 3. the supervisor's own restart decision (mtime diff on the watched
//!    paths, binary-change override) — unchanged from the single-command era.
//!
//! Guardrails shared by every trigger:
//! - `--pause-file PATH`: while the file exists, triggers are skipped. This
//!   is the manual-deploy handshake: an operator touches the file, does a
//!   hand deploy, removes the file.
//! - `--build-lock PATH`: an exclusive flock held for the pipeline's
//!   duration, so several supervised units (or an external build script)
//!   sharing one source checkout cannot interleave builds.
//! - failure backoff: consecutive pipeline failures delay the next attempt
//!   exponentially (30 s, 60 s, … capped at 15 min); one success resets it.

use std::fs::File;
use std::path::{Path, PathBuf};
use std::process::Stdio;
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use tokio::io::{AsyncBufReadExt, BufReader};
use tokio::process::Command as TokioCommand;
use tokio::sync::mpsc;
use tracing::{info, warn};

/// What woke the pipeline. File triggers carry the changed paths (used for
/// the supervised-binary override); remote triggers carry the new upstream
/// SHA (exported to stages/install as `MALKUTH_REMOTE_SHA`).
#[derive(Debug, Clone)]
pub enum Trigger {
    Files(Vec<PathBuf>),
    Remote(String),
}

impl Trigger {
    /// Value of the `MALKUTH_TRIGGER` env var handed to stages and install.
    pub fn kind(&self) -> &'static str {
        match self {
            Trigger::Files(_) => "files",
            Trigger::Remote(_) => "remote",
        }
    }
}

/// One build step. `name` is informational (info page + logs); `None` is the
/// legacy anonymous `--build` stage (raw output, no `[name]` prefix). `cmd`
/// runs under `sh -c` like the legacy `--build` string.
#[derive(Debug, Clone)]
pub struct BuildStage {
    pub name: Option<String>,
    pub cmd: String,
}

impl BuildStage {
    /// Parse a `NAME=CMD` spec. The first `=` splits; the name must be
    /// non-empty (the command may contain further `=`).
    pub fn parse(spec: &str) -> Result<Self, String> {
        let (name, cmd) = spec
            .split_once('=')
            .ok_or_else(|| format!("--build-stage must look like NAME=CMD (got `{spec}`)"))?;
        let name = name.trim();
        if name.is_empty() {
            return Err(format!(
                "--build-stage name must not be empty (got `{spec}`)"
            ));
        }
        Ok(Self {
            name: Some(name.to_string()),
            cmd: cmd.to_string(),
        })
    }
}

/// Parse a `URL#REF` remote spec (e.g. `https://host/org/repo.git#master`).
/// Local checkout paths work too — `git ls-remote` accepts them.
pub fn parse_remote_spec(spec: &str) -> Result<(String, String), String> {
    let (url, r#ref) = spec
        .split_once('#')
        .ok_or_else(|| format!("--watch-remote must look like URL#REF (got `{spec}`)"))?;
    if url.is_empty() || r#ref.is_empty() {
        return Err(format!(
            "--watch-remote URL and REF must be non-empty (got `{spec}`)"
        ));
    }
    Ok((url.to_string(), r#ref.to_string()))
}

/// Exponential backoff across consecutive pipeline failures.
///
/// `blocked_until` semantics: a trigger arriving before the instant is
/// dropped (logged) — the next file change or remote poll retries later.
#[derive(Debug)]
pub struct FailureBackoff {
    base: Duration,
    cap: Duration,
    consecutive: u32,
    blocked_until: Option<Instant>,
}

impl FailureBackoff {
    const DEFAULT_BASE: Duration = Duration::from_secs(30);
    const DEFAULT_CAP: Duration = Duration::from_secs(15 * 60);

    pub fn new() -> Self {
        Self {
            base: Self::DEFAULT_BASE,
            cap: Self::DEFAULT_CAP,
            consecutive: 0,
            blocked_until: None,
        }
    }

    /// Delay applied after the Nth consecutive failure: base * 2^(N-1),
    /// capped. Returns the delay that was applied.
    pub fn record_failure(&mut self) -> Duration {
        self.consecutive = self.consecutive.saturating_add(1);
        let shift = self.consecutive.saturating_sub(1).min(16);
        let delay = self.base.saturating_mul(1u32 << shift).min(self.cap);
        self.blocked_until = Instant::now().checked_add(delay);
        delay
    }

    pub fn record_success(&mut self) {
        self.consecutive = 0;
        self.blocked_until = None;
    }

    pub fn is_blocked(&self) -> bool {
        self.blocked_until
            .is_some_and(|until| until > Instant::now())
    }
}

impl Default for FailureBackoff {
    fn default() -> Self {
        Self::new()
    }
}

/// Outcome of acquiring the cross-unit build lock.
#[derive(Debug)]
pub enum LockOutcome {
    /// Lock taken; keep the file handle alive for the pipeline's duration
    /// (dropping it releases the flock).
    Acquired(File),
    /// Another process holds the lock — skip this trigger.
    Busy,
    /// The lock file itself could not be opened (permissions, missing
    /// directory). Proceeding WITHOUT the lock: freezing every trigger over a
    /// lock-path typo would silently disable deploys, which is worse than an
    /// occasionally interleaved build.
    Disabled,
}

/// Try to take the exclusive build lock at `path` (non-blocking).
pub fn acquire_build_lock(path: &Path) -> LockOutcome {
    #[cfg(unix)]
    {
        use std::os::unix::io::AsRawFd;
        let file = match File::options()
            .read(true)
            .write(true)
            .create(true)
            .truncate(false)
            .open(path)
        {
            Ok(f) => f,
            Err(e) => {
                warn!(
                    path = %path.display(),
                    error = %e,
                    "cannot open build lock, proceeding unlocked"
                );
                return LockOutcome::Disabled;
            }
        };
        let rc = unsafe { libc::flock(file.as_raw_fd(), libc::LOCK_EX | libc::LOCK_NB) };
        if rc == 0 {
            LockOutcome::Acquired(file)
        } else {
            info!(path = %path.display(), "build lock busy, skipping trigger");
            LockOutcome::Busy
        }
    }
    #[cfg(not(unix))]
    {
        let _ = path;
        LockOutcome::Disabled
    }
}

/// Why a stage (or the install hook) did not complete successfully.
#[derive(Debug)]
pub enum StageFailure {
    /// The command ran and exited non-zero.
    Exit(String),
    /// The command could not be spawned / reaped.
    Spawn(String),
}

impl std::fmt::Display for StageFailure {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            StageFailure::Exit(s) => write!(f, "exit {s}"),
            StageFailure::Spawn(s) => write!(f, "spawn error: {s}"),
        }
    }
}

/// Run one shell command, streaming stdout lines into the shared info-page
/// progress/log cells (the legacy single-command behavior). Named stages
/// prefix each line with `[name] ` so the info page shows the pipeline's
/// position; the anonymous legacy stage stays unprefixed.
///
/// `envs` are added to the child environment (trigger metadata for scripts).
pub async fn run_shell_stage(
    stage: Option<&BuildStage>,
    cmd: &str,
    envs: &[(&str, String)],
    progress: &Arc<Mutex<Option<String>>>,
    log_lines: &Arc<Mutex<Vec<String>>>,
) -> Result<(), StageFailure> {
    let mut command = TokioCommand::new("sh");
    command.arg("-c").arg(cmd);
    for (k, v) in envs {
        command.env(k, v);
    }
    let child = command
        .stdout(Stdio::piped())
        .stderr(Stdio::inherit())
        .spawn();
    let mut child = match child {
        Ok(c) => c,
        Err(e) => return Err(StageFailure::Spawn(e.to_string())),
    };
    let tag = stage
        .and_then(|s| s.name.as_ref())
        .map(|n| format!("[{n}] "));
    if let Some(stdout) = child.stdout.take() {
        let mut lines = BufReader::new(stdout).lines();
        loop {
            tokio::select! {
                line = lines.next_line() => {
                    match line {
                        Ok(Some(l)) => {
                            let t = l.trim().to_string();
                            if !t.is_empty() {
                                let shown = match &tag {
                                    Some(tag) => format!("{tag}{t}"),
                                    None => t.clone(),
                                };
                                if let Ok(mut g) = log_lines.lock() {
                                    g.push(shown.clone());
                                    if g.len() > 50 {
                                        g.remove(0);
                                    }
                                }
                                if let Ok(mut g) = progress.lock() {
                                    *g = Some(shown);
                                }
                            }
                        }
                        _ => break,
                    }
                }
                _ = tokio::signal::ctrl_c() => { break; }
            }
        }
    }
    let status = child.wait().await;
    if let Ok(mut g) = progress.lock() {
        *g = None;
    }
    match status {
        Ok(s) if s.success() => Ok(()),
        Ok(s) => Err(StageFailure::Exit(s.to_string())),
        Err(e) => Err(StageFailure::Spawn(e.to_string())),
    }
}

/// Spawn the upstream poll task: every `poll_secs`, resolve `ref` on `url`
/// via `git ls-remote`; a SHA different from the previous sample fires one
/// `Trigger::Remote`. The first successful sample only primes the baseline
/// (no build on startup). Probe failures warn and keep the old baseline.
pub fn spawn_remote_poller(
    spec: &str,
    poll_secs: u64,
    tx: mpsc::Sender<Trigger>,
) -> Result<(), String> {
    let (url, r#ref) = parse_remote_spec(spec)?;
    let interval = Duration::from_secs(poll_secs.max(10));
    tokio::spawn(async move {
        let mut baseline: Option<String> = None;
        loop {
            match probe_remote(&url, &r#ref).await {
                Ok(sha) => {
                    if let Some(prev) = &baseline {
                        if *prev != sha {
                            info!(url = %url, r#ref, from = %prev, to = %sha, "remote ref moved");
                            if tx.send(Trigger::Remote(sha.clone())).await.is_err() {
                                break;
                            }
                        }
                    }
                    baseline = Some(sha);
                }
                Err(e) => {
                    warn!(url = %url, r#ref, error = %e, "remote probe failed");
                }
            }
            tokio::time::sleep(interval).await;
        }
    });
    Ok(())
}

/// Resolve `ref` on `url` to its current SHA (first tab-separated field of
/// the first `git ls-remote` line).
async fn probe_remote(url: &str, r#ref: &str) -> Result<String, String> {
    let out = TokioCommand::new("git")
        .arg("ls-remote")
        .arg(url)
        .arg(r#ref)
        .stdout(Stdio::piped())
        .stderr(Stdio::null())
        .output()
        .await
        .map_err(|e| e.to_string())?;
    if !out.status.success() {
        return Err(format!("git ls-remote exited {}", out.status));
    }
    let stdout = String::from_utf8_lossy(&out.stdout);
    stdout
        .lines()
        .find_map(|l| l.split('\t').next().map(|s| s.trim().to_string()))
        .filter(|s| !s.is_empty())
        .ok_or_else(|| "ls-remote returned no ref".to_string())
}

/// Convenience wrapper matching the watcher's receiver into `Trigger::Files`
/// so the pipeline consumer sees one merged stream.
pub fn forward_file_triggers(mut files: mpsc::Receiver<Vec<PathBuf>>, tx: mpsc::Sender<Trigger>) {
    tokio::spawn(async move {
        while let Some(paths) = files.recv().await {
            if tx.send(Trigger::Files(paths)).await.is_err() {
                break;
            }
        }
    });
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn build_stage_parse_splits_on_first_equals() {
        let s = BuildStage::parse("web=vite build --mode=prod").unwrap();
        assert_eq!(s.name.as_deref(), Some("web"));
        assert_eq!(s.cmd, "vite build --mode=prod");
        // Empty names rejected; missing `=` rejected; empty cmd allowed (sh
        // succeeds) but the name must survive trimming.
        assert!(BuildStage::parse("=cmd").is_err());
        assert!(BuildStage::parse("nonsense").is_err());
        assert_eq!(
            BuildStage::parse("  padded  = cmd")
                .unwrap()
                .name
                .as_deref(),
            Some("padded")
        );
    }

    #[test]
    fn remote_spec_parse() {
        let (url, r#ref) = parse_remote_spec("https://github.com/org/repo.git#master").unwrap();
        assert_eq!(url, "https://github.com/org/repo.git");
        assert_eq!(r#ref, "master");
        // Local checkout paths work with ls-remote too.
        let (url, r#ref) = parse_remote_spec("/mnt/codespace/repo#refs/heads/master").unwrap();
        assert_eq!(url, "/mnt/codespace/repo");
        assert_eq!(r#ref, "refs/heads/master");
        assert!(parse_remote_spec("no-ref-here").is_err());
        assert!(parse_remote_spec("url#").is_err());
        assert!(parse_remote_spec("#ref").is_err());
    }

    #[test]
    fn backoff_doubles_then_caps_and_resets() {
        let mut b = FailureBackoff::new();
        assert!(!b.is_blocked());
        assert_eq!(b.record_failure(), Duration::from_secs(30));
        assert!(b.is_blocked());
        assert_eq!(b.record_failure(), Duration::from_secs(60));
        assert_eq!(b.record_failure(), Duration::from_secs(120));
        for _ in 0..10 {
            b.record_failure();
        }
        // Capped, no overflow panic.
        assert_eq!(b.record_failure(), FailureBackoff::DEFAULT_CAP);
        b.record_success();
        assert!(!b.is_blocked());
        assert_eq!(b.record_failure(), Duration::from_secs(30));
    }

    #[test]
    fn build_lock_is_exclusive() {
        let dir = std::env::temp_dir().join(format!(
            "malkuth-lock-{}-{:?}",
            std::process::id(),
            std::thread::current().id()
        ));
        std::fs::create_dir_all(&dir).unwrap();
        let lock = dir.join("build.lock");
        let _ = std::fs::remove_file(&lock);

        let first = acquire_build_lock(&lock);
        assert!(matches!(first, LockOutcome::Acquired(_)));
        // While held, a second acquire reports busy…
        assert!(matches!(acquire_build_lock(&lock), LockOutcome::Busy));
        // …and releasing (drop) makes it available again.
        drop(first);
        assert!(matches!(
            acquire_build_lock(&lock),
            LockOutcome::Acquired(_)
        ));
        // An un-openable lock path (missing parent dir) reports Disabled —
        // the pipeline proceeds unlocked rather than freezing deploys.
        assert!(matches!(
            acquire_build_lock(Path::new("/nonexistent-dir-xyz/build.lock")),
            LockOutcome::Disabled
        ));
        let _ = std::fs::remove_file(&lock);
        let _ = std::fs::remove_dir(&dir);
    }

    #[tokio::test]
    async fn shell_stage_streams_lines_and_reports_failures() {
        let progress = Arc::new(Mutex::new(None));
        let log = Arc::new(Mutex::new(Vec::new()));
        let stage = BuildStage {
            name: Some("web".into()),
            cmd: "echo hello-stage".into(),
        };
        run_shell_stage(Some(&stage), &stage.cmd, &[], &progress, &log)
            .await
            .unwrap();
        assert_eq!(
            log.lock().unwrap().last().map(String::as_str),
            Some("[web] hello-stage")
        );
        assert_eq!(progress.lock().unwrap().take(), None);

        // The legacy anonymous stage keeps its raw lines (no [name] prefix).
        let anon = BuildStage {
            name: None,
            cmd: "echo raw".into(),
        };
        run_shell_stage(Some(&anon), &anon.cmd, &[], &progress, &log)
            .await
            .unwrap();
        assert_eq!(log.lock().unwrap().last().map(String::as_str), Some("raw"));

        // Non-zero exit is an Exit failure.
        let err = run_shell_stage(None, "echo boom >&2; exit 3", &[], &progress, &log)
            .await
            .unwrap_err();
        assert!(matches!(err, StageFailure::Exit(_)));

        // Env vars reach the child.
        let err = run_shell_stage(
            None,
            "test \"$MALKUTH_TRIGGER\" = remote",
            &[("MALKUTH_TRIGGER", "remote".to_string())],
            &progress,
            &log,
        )
        .await;
        assert!(err.is_ok());
    }

    #[tokio::test]
    async fn remote_probe_reads_local_repo() {
        // A throwaway local git repo: HEAD resolves and changes on commit.
        let dir = std::env::temp_dir().join(format!(
            "malkuth-remote-{}-{:?}",
            std::process::id(),
            std::thread::current().id()
        ));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        let run = |args: &[&str]| {
            std::process::Command::new("git")
                .args(args)
                .current_dir(&dir)
                .output()
                .unwrap()
        };
        if !run(&["init", "-q"]).status.success() {
            return; // git unavailable in this environment; skip
        }
        run(&["config", "user.email", "t@t"]);
        run(&["config", "user.name", "t"]);
        std::fs::write(dir.join("f"), b"1").unwrap();
        run(&["add", "."]);
        run(&["commit", "-qm", "one"]);
        let url = dir.to_string_lossy().to_string();
        let first = probe_remote(&url, "HEAD").await.unwrap();
        std::fs::write(dir.join("f"), b"2").unwrap();
        run(&["commit", "-qam", "two"]);
        let second = probe_remote(&url, "HEAD").await.unwrap();
        assert_ne!(first, second);
        let _ = std::fs::remove_dir_all(&dir);
    }
}
