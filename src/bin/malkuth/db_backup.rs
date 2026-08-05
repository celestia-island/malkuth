//! Automatic database backup facility for the malkuth watchdog.
//!
//! When `--db-backup-dir DIR` is set, the watchdog backs up every database
//! URL it can discover — explicit `--db-backup-uri` values plus the process
//! environment's `DATABASE_URL` / `*_DATABASE_URL` (which systemd units
//! naturally carry via `Environment=`) — into DIR using `pg_dump -Fc`
//! (custom format, restorable with `pg_restore -Fc`).
//!
//! A rolling window keeps the newest `--db-backup-retain N` dumps per
//! database. When `--db-backup-key` is set, every dump is encrypted at rest
//! with `age -r <recipient>` (`.age` suffix) — the default posture for
//! compliance-sensitive data such as real-name or report records.

use std::collections::HashSet;
use std::path::PathBuf;
use std::process::Command;
use std::time::Duration;

use chrono::Utc;
use tracing::{error, info, warn};

/// Configuration assembled from the watchdog CLI flags.
#[derive(Debug, Clone)]
pub struct DbBackupConfig {
    pub dir: PathBuf,
    pub retain: usize,
    pub key: Option<String>,
    pub uris: Vec<String>,
}

/// Collect the database URLs to back up: explicit values first, then every
/// `DATABASE_URL` / `*_DATABASE_URL` variable from the process environment.
/// Duplicates are collapsed.
pub fn discover_uris(explicit: &[String]) -> Vec<String> {
    let mut found = HashSet::new();
    for u in explicit {
        if !u.trim().is_empty() {
            found.insert(u.clone());
        }
    }
    for (k, v) in std::env::vars() {
        if k == "DATABASE_URL" || k.ends_with("_DATABASE_URL") {
            found.insert(v);
        }
    }
    let mut out: Vec<String> = found.into_iter().collect();
    out.sort();
    out
}

/// Extract the database name from a postgres URL (`postgres://u:p@host/db`).
/// Falls back to the sanitized host when the path segment is empty.
pub fn db_name_from_uri(uri: &str) -> String {
    let after_slash = uri.rfind('/').map(|i| &uri[i + 1..]).unwrap_or("");
    let name = after_slash.split('?').next().unwrap_or("");
    if name.is_empty() {
        let host = uri
            .strip_prefix("postgres://")
            .or_else(|| uri.strip_prefix("postgresql://"))
            .unwrap_or(uri);
        let host = host
            .split('@')
            .next_back()
            .unwrap_or(host)
            .split(':')
            .next()
            .unwrap_or(host);
        sanitize_label(host)
    } else {
        sanitize_label(name)
    }
}

/// Reduce arbitrary strings to a filesystem-safe label (alphanumerics, `-`, `_`).
fn sanitize_label(s: &str) -> String {
    let mut out = String::with_capacity(s.len());
    for c in s.chars() {
        if c.is_ascii_alphanumeric() || c == '-' || c == '_' {
            out.push(c);
        } else {
            out.push('_');
        }
    }
    out
}

/// Run one backup of `uri` into `dir`. Returns the final dump path.
pub fn run_backup(cfg: &DbBackupConfig, uri: &str) -> Result<PathBuf, String> {
    let dir = &cfg.dir;
    std::fs::create_dir_all(dir)
        .map_err(|e| format!("create backup dir {}: {e}", dir.display()))?;
    // Backups may hold compliance-sensitive rows; keep the directory private.
    use std::os::unix::fs::PermissionsExt;
    let _ = std::fs::set_permissions(dir, std::fs::Permissions::from_mode(0o700));

    let host = sanitize_label(&hostname());
    let db = db_name_from_uri(uri);
    let stamp = Utc::now().format("%Y%m%d-%H%M%S");
    let tmp = dir.join(format!(".{host}-{db}-{stamp}.tmp"));
    let final_path = if cfg.key.is_some() {
        dir.join(format!("{host}-{db}-{stamp}.dump.age"))
    } else {
        dir.join(format!("{host}-{db}-{stamp}.dump"))
    };

    let out = Command::new("pg_dump")
        .arg("-Fc")
        .arg("--file")
        .arg(&tmp)
        .arg(uri)
        .output()
        .map_err(|e| format!("spawn pg_dump: {e}"))?;
    if !out.status.success() {
        let _ = std::fs::remove_file(&tmp);
        let stderr = String::from_utf8_lossy(&out.stderr);
        return Err(format!(
            "pg_dump failed for {db}: {}",
            stderr.trim().lines().last().unwrap_or("unknown error")
        ));
    }

    if let Some(ref key) = cfg.key {
        let enc = Command::new("age")
            .arg("-r")
            .arg(key)
            .arg("-o")
            .arg(&final_path)
            .arg(&tmp)
            .output();
        if let Err(e) = enc {
            let _ = std::fs::remove_file(&tmp);
            return Err(format!("spawn age: {e}"));
        }
        let enc = enc.expect("checked above");
        let _ = std::fs::remove_file(&tmp);
        if !enc.status.success() {
            let _ = std::fs::remove_file(&final_path);
            let stderr = String::from_utf8_lossy(&enc.stderr);
            return Err(format!(
                "age encryption failed: {}",
                stderr.trim().lines().last().unwrap_or("unknown error")
            ));
        }
    } else if let Err(e) = std::fs::rename(&tmp, &final_path) {
        let _ = std::fs::remove_file(&tmp);
        return Err(format!("finalize dump: {e}"));
    }

    info!(path = %final_path.display(), db, "database backup created");
    Ok(final_path)
}

/// Trim the backup directory to the newest `retain` dumps per database.
pub fn rotate(cfg: &DbBackupConfig) {
    let Ok(entries) = std::fs::read_dir(&cfg.dir) else {
        return;
    };
    let mut by_prefix: std::collections::HashMap<String, Vec<PathBuf>> =
        std::collections::HashMap::new();
    for e in entries.flatten() {
        let name = e.file_name().to_string_lossy().into_owned();
        if !name.ends_with(".dump") && !name.ends_with(".dump.age") {
            continue;
        }
        // Group by `<host>-<db>`: strip the trailing `-<timestamp>` segment.
        let base = name.strip_suffix(".age").unwrap_or(&name);
        let base = base.strip_suffix(".dump").unwrap_or(base);
        let prefix = match base.rfind('-') {
            Some(i) => base[..i].to_string(),
            None => base.to_string(),
        };
        by_prefix.entry(prefix).or_default().push(e.path());
    }
    for (_prefix, mut files) in by_prefix {
        files.sort();
        if files.len() > cfg.retain {
            for stale in files.iter().take(files.len() - cfg.retain) {
                if std::fs::remove_file(stale).is_ok() {
                    info!(path = %stale.display(), "rotated out stale dump");
                }
            }
        }
    }
}

/// Back up every discovered database immediately, then once per day at 02:00
/// local time. Runs until the process exits; failures are logged only.
pub async fn run_forever(cfg: DbBackupConfig) {
    let uris = discover_uris(&cfg.uris);
    if uris.is_empty() {
        warn!(dir = %cfg.dir.display(), "db-backup enabled but no database URLs discovered");
    }
    loop {
        for uri in &uris {
            match run_backup(&cfg, uri) {
                Ok(_) => {}
                Err(e) => error!(uri, error = %e, "database backup failed"),
            }
        }
        rotate(&cfg);
        tokio::time::sleep(sleep_until_next_run()).await;
    }
}

/// Duration until the next 02:00 local time (at least 60s from now).
fn sleep_until_next_run() -> Duration {
    let now = chrono::Local::now();
    let next = now
        .date_naive()
        .and_hms_opt(2, 0, 0)
        .and_then(|d| d.and_local_timezone(chrono::Local).single())
        .map(|dt| if dt > now { dt } else { dt + chrono::Duration::days(1) })
        .unwrap_or(now + chrono::Duration::hours(24));
    let secs = (next - now).num_seconds().max(60) as u64;
    Duration::from_secs(secs)
}

fn hostname() -> String {
    std::env::var("HOSTNAME")
        .or_else(|_| std::env::var("HOST"))
        .unwrap_or_else(|_| "host".to_string())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn discovers_explicit_and_env_urls() {
        let explicit = vec!["postgres://a/b".to_string()];
        // SAFETY: scoped env mutation; single-threaded test.
        unsafe {
            std::env::set_var("DATABASE_URL", "postgres://x/y");
            std::env::set_var("CHEST_DATABASE_URL", "postgres://c/d");
            std::env::set_var("UNRELATED", "noise");
        }
        let uris = discover_uris(&explicit);
        assert!(uris.contains(&"postgres://a/b".to_string()));
        assert!(uris.contains(&"postgres://x/y".to_string()));
        assert!(uris.contains(&"postgres://c/d".to_string()));
        assert_eq!(uris.len(), 3);
        unsafe {
            std::env::remove_var("DATABASE_URL");
            std::env::remove_var("CHEST_DATABASE_URL");
            std::env::remove_var("UNRELATED");
        }
    }

    #[test]
    fn parses_db_names() {
        assert_eq!(db_name_from_uri("postgres://u:p@h:5432/arona"), "arona");
        assert_eq!(db_name_from_uri("postgres://h/chest?sslmode=require"), "chest");
        // No path segment: fall back to a sanitized host label.
        assert_eq!(db_name_from_uri("postgres://dbhost:5432/"), "dbhost");
    }

    #[test]
    fn rotates_to_retain_count() {
        let dir = std::env::temp_dir().join(format!("malkuth-dbbackup-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        for i in 0..5 {
            std::fs::write(dir.join(format!("host-db-20260805-0000{i}.dump")), b"x").unwrap();
        }
        let cfg = DbBackupConfig {
            dir: dir.clone(),
            retain: 3,
            key: None,
            uris: vec![],
        };
        rotate(&cfg);
        let left: Vec<String> = std::fs::read_dir(&dir)
            .unwrap()
            .flatten()
            .map(|e| e.file_name().to_string_lossy().into_owned())
            .filter(|n| n.ends_with(".dump"))
            .collect();
        assert_eq!(left.len(), 3, "kept {left:?}");
        let _ = std::fs::remove_dir_all(&dir);
    }
}
