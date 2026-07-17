# malkuth — composable service-supervision toolkit (tokio).

set shell := ["bash", "-c"]
set windows-shell := ["bash.exe", "-c"]
set unstable
set lists

# Shared celestia-devtools recipes — NOT in git. This justfile references shared
# variables, so the import is REQUIRED. Bootstrap once: celestia-devtools init
# (or `just fetch` if already staged). Refresh after upgrades.
import? "./.just/git-bash-interop.just"
import "./.just/celestia-devtools.just"

# Stage shared celestia-devtools recipes into .just/ (gitignored).
# Source order: explicit URL arg → local pip bundle (offline) → GitHub raw.
# curl honors HTTP_PROXY/HTTPS_PROXY/ALL_PROXY env vars automatically.
[script('bash')]
fetch URL='':
    #!/usr/bin/env bash
    set -euo pipefail
    out=.just/celestia-devtools.just
    mkdir -p .just
    if [ -n "{{URL}}" ]; then
      echo "[fetch] {{URL}} -> $out"
      curl -fsSL "{{URL}}" -o "$out"
    elif command -v celestia-devtools >/dev/null 2>&1; then
      src=$(celestia-devtools include-path)
      echo "[fetch] local bundle ($src) -> $out"
      cp "$src" "$out"
    else
      echo "[fetch] github raw -> $out"
      curl -fsSL "https://raw.githubusercontent.com/celestia-island/celestia-devtools/dev/src/celestia_devtools/common.just" -o "$out"
    fi
    echo "[fetch] wrote $out"

default:
    @just --list

# Format all sources.
fmt:
    cargo fmt --all

# Check formatting without writing.
fmt-check:
    cargo fmt --all -- --check

# Type-check all targets and features.
check:
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
