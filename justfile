# malkuth — composable service-supervision toolkit (tokio).

set shell := ["bash", "-c"]
# Windows: PowerShell (the 5.1 floor ships with every Windows; pwsh 7 is
# NOT assumed). Linewise recipes must stay PS-5.1-safe: no `&&` chains,
# `cd X; cmd` instead of `cd X && cmd`. Bash-only recipes use
# [script('bash')] and need Git Bash (or WSL) when actually run.
set windows-shell := ["powershell.exe", "-NoLogo", "-NoProfile", "-Command", "[Console]::OutputEncoding=[System.Text.Encoding]::UTF8; $PSDefaultParameterValues['*:Encoding']='utf8';"]
set unstable
set lists

# Repo definitions override the shared template's (imported above).
set allow-duplicate-recipes
set allow-duplicate-variables

# Shared celestia-devtools recipes — NOT in git. This justfile references shared
# variables, so the import is REQUIRED. Bootstrap once: celestia-devtools init
# (or `just fetch` if already staged). Refresh after upgrades.
python_cmd := "python3"
import? "./.just/celestia-devtools.just"

default:
    @just --list

# Format all sources.
fmt:
    just fmt-toml
    cargo fmt --all

# Check formatting without writing.
fmt-check:
    cargo fmt --all -- --check

# Type-check all targets and features (format gate included).
check: fmt-check
    cargo check --all-targets --all-features

# Clippy with -D warnings.
clippy:
    cargo clippy --all-targets --all-features -- -D warnings

# Run the Rust unit/integration test suite.
test:
    cargo test --all-features

# Build the binaries used by the Python integration tests.
build-bins:
    cargo build --features cli --features worker
    cargo build --example test_app --features tcp,worker,signals

# Run the Python scripts/ integration suite (CLI + test-app scenarios).
test-cli: build-bins
    {{python_cmd}} scripts/tests/run_all.py

# Build all features.
build:
    cargo build --all-features

# One-shot local gate: fmt-check + clippy + cargo tests + python integration tests.
ci:
    just fmt-check
    just clippy
    just test
    just test-cli

# ── npx distribution (local dry-run) ─────────────────────────────────────────
#
# Wraps the shared recipe from celestia-devtools.just with malkuth's metadata.
# CI does the actual publish (see .github/workflows/npm-release.yml); locally
# this only stages ./dist and runs `npm pack --dry-run`.
#
#   just npm-dist-local                                           # reassemble root from existing dist/
#   just npm-dist-local 0.1.0 path/to/malkuth x86_64-pc-windows-msvc
npm-dist-local version='' binary='' target='':
    just npm-dist malkuth {{version}} {{binary}} {{target}}

# ── Landing page artifacts (committed so node-less builds still work) ──────
#
# The landing page is a build-time product: the SPA is inlined into one HTML
# file, the same hikari component tree is server-rendered into the no-JS
# fallback page, and the theme snapshot feeds the 503 restart page. build.rs
# reads target/landing_page/ when pnpm ran; otherwise it reads the committed
# copies under src/info_page/ — regenerate those here.
gen-pages:
    cd packages/landing_page; pnpm build
    cp target/landing_page/fallback.html src/info_page/fallback_page.html
    cp target/landing_page/hikari-tokens.json src/info_page/design_tokens.json

# NOTE: the emitted CSS is not byte-reproducible (the minifier may order
# equal-length declarations differently between runs), so this is a
# regenerate-and-review step, not a git-diff gate. build.rs always prefers
# target/landing_page/ when pnpm ran; the committed copies only matter for
# node-less builds (CI). Re-run this after touching packages/landing_page.
