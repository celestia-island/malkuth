/// Build the landing page and embed it as a Rust source file.
/// If pnpm is not available or the build fails, a minimal fallback is used.
fn main() {
    let landing_dir = std::path::Path::new("packages/landing_page");
    let dist = std::path::Path::new("target/landing_page/index.html");
    let out_dir = std::env::var("OUT_DIR").unwrap();
    let gen_path = format!("{}/landing_page_html.rs", out_dir);

    println!("cargo:rerun-if-changed=packages/landing_page/src");

    if !dist.exists() && landing_dir.join("package.json").exists() {
        let pnpm = std::env::var("PNPM_HOME")
            .map(|h| format!("{}/pnpm", h))
            .unwrap_or_else(|_| "pnpm".into());
        // Also try common nvm paths
        let pnpm_candidates = [
            format!("{}/.nvm/versions/node/v24.18.0/bin/pnpm", std::env::var("HOME").unwrap_or_default()),
            pnpm,
        ];
        let mut built = false;
        for pnpm_path in &pnpm_candidates {
            let status = std::process::Command::new(pnpm_path)
                .args(["build"])
                .current_dir(landing_dir)
                .status();
            if let Ok(s) = status {
                if s.success() {
                    println!("cargo:warning=landing_page built successfully with {}", pnpm_path);
                    built = true;
                    break;
                }
            }
        }
        if !built {
            println!("cargo:warning=landing_page build skipped");
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
