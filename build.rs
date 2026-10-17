/// Build the landing page and embed it as a Rust source file.
/// A pnpm that exists but fails ABORTS the build (a failed vite build
/// still rewrites dist with a fresh mtime, so a stale embed could never
/// be caught by timestamps); hosts without pnpm fall back to the
/// existing dist or a minimal page, guarded by a best-effort mtime check.
fn main() {
    let landing_dir = std::path::Path::new("packages/landing_page");
    let dist = std::path::Path::new("target/landing_page/index.html");
    let out_dir = std::env::var("OUT_DIR").unwrap();
    let gen_path = format!("{}/landing_page_html.rs", out_dir);

    println!("cargo:rerun-if-changed=packages/landing_page/src");
    // The embed also depends on files OUTSIDE src/: the entry html, the
    // vite config and the package manifests. Without these, hand edits to
    // those files never refreshed the embedded build (R2' catch).
    for extra in [
        "index.html",
        "package.json",
        "vite.config.ts",
        "tsconfig.json",
        "pnpm-lock.yaml",
    ] {
        println!("cargo:rerun-if-changed=packages/landing_page/{extra}");
    }

    // Always attempt the build when the package is present: gating on
    // `!dist.exists()` embedded a STALE target/landing_page/index.html
    // whenever the sources changed but an old dist lingered (R1 catch —
    // the embedded SPA silently lagged the sources).
    if landing_dir.join("package.json").exists() {
        let pnpm = std::env::var("PNPM_HOME")
            .map(|h| format!("{}/pnpm", h))
            .unwrap_or_else(|_| "pnpm".into());
        // Also try common nvm paths
        let pnpm_candidates = [
            format!(
                "{}/.nvm/versions/node/v24.18.0/bin/pnpm",
                std::env::var("HOME").unwrap_or_default()
            ),
            pnpm,
        ];
        let mut built = false;
        let mut attempted = false;
        for pnpm_path in &pnpm_candidates {
            let status = std::process::Command::new(pnpm_path)
                .args(["build"])
                .current_dir(landing_dir)
                .status();
            if let Ok(s) = status {
                attempted = true;
                if s.success() {
                    println!(
                        "cargo:warning=landing_page built successfully with {}",
                        pnpm_path
                    );
                    built = true;
                    break;
                }
            }
        }
        if !built {
            if attempted {
                // A pnpm that EXISTS but FAILED must abort the build: vite's
                // closeBundle rewrites dist (stale content, FRESH mtime)
                // even on a failed build, so the mtime guard below cannot
                // catch this and the embed would silently pin the last
                // good build (R3' catch).
                panic!(
                    "landing_page build was attempted and FAILED — refusing to embed a \
                     possibly stale dist (vite rewrites dist with a fresh mtime even on \
                     failure)"
                );
            }
            println!("cargo:warning=landing_page build skipped (no pnpm on this host)");
        }
    }

    // Freshness guard (R2' catch): a failed rebuild must NOT silently
    // embed a dist older than the landing-page sources — that kept the
    // contract tests green against the last good build. Hosts without
    // pnpm that never touched the sources still embed their fresh dist;
    // a missing dist keeps the historical minimal fallback.
    if dist.exists() {
        let dist_mtime = std::fs::metadata(dist).and_then(|m| m.modified()).ok();
        let youngest_src = youngest_mtime(&landing_dir.join("src"));
        if let (Some(dist_mtime), Some(youngest_src)) = (dist_mtime, youngest_src) {
            if dist_mtime < youngest_src {
                panic!(
                    "landing_page dist is STALE (older than packages/landing_page/src) and the \
                     rebuild did not succeed — refusing to embed a build that lags the sources"
                );
            }
        }
    }

    let html = if dist.exists() {
        std::fs::read_to_string(dist).unwrap_or_default()
    } else {
        "<html><body><h1>Malkuth</h1><p>Landing page not built.</p></body></html>".into()
    };

    let escaped = html
        .replace('\\', "\\\\")
        .replace('"', "\\\"")
        .replace('\n', "\\n")
        .replace('\r', "");

    std::fs::write(
        &gen_path,
        format!("pub const LANDING_PAGE_HTML: &str = \"{}\";\n", escaped),
    )
    .expect("Failed to write landing page HTML source");
}

/// Newest mtime under `dir` (recursive, best effort).
fn youngest_mtime(dir: &std::path::Path) -> Option<std::time::SystemTime> {
    let mut newest: Option<std::time::SystemTime> = None;
    let mut stack = vec![dir.to_path_buf()];
    while let Some(d) = stack.pop() {
        let Ok(entries) = std::fs::read_dir(&d) else {
            continue;
        };
        for entry in entries.flatten() {
            let Ok(md) = entry.metadata() else { continue };
            if md.is_dir() {
                stack.push(entry.path());
            } else if let Ok(m) = md.modified() {
                if newest.is_none_or(|n| m > n) {
                    newest = Some(m);
                }
            }
        }
    }
    newest
}
