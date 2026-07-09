# malkuth — composable service-supervision toolkit (tokio).

set shell := ["bash", "-c"]
# On Windows just resolves recipe shebangs through the shell named here; without
# it just falls back to `cygpath`, which Git for Windows does not put on PATH,
# so every shebang recipe fails with "could not find cygpath executable".
set windows-shell := ["bash.exe", "-c"]
# `set lists` enables which() (used by the imported celestia-devtools.just);
# `set unstable` gates it.
set unstable
set lists

import "./celestia-devtools.just"

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
