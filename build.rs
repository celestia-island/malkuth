/// Try to build the landing page before compiling.
/// If pnpm is not available or the build fails, the Tera fallback is used.
fn main() {
    let landing_dir = std::path::Path::new("packages/landing_page");
    if !landing_dir.join("package.json").exists() {
        return;
    }
    // Check if dist already exists and is fresh
    let dist = std::path::Path::new("target/landing_page/index.html");
    if dist.exists() {
        println!("cargo:warning=landing_page dist already built");
        return;
    }
    // Try pnpm build
    let status = std::process::Command::new("pnpm")
        .args(["build"])
        .current_dir(landing_dir)
        .status();
    match status {
        Ok(s) if s.success() => println!("cargo:warning=landing_page built successfully"),
        _ => println!("cargo:warning=landing_page build skipped (no pnpm or build failed)"),
    }
}
