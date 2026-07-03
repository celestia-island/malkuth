# malkuth — composable service-supervision toolkit (tokio).

set shell := ["bash", "-c"]
python_cmd := if os_family() == "windows" {
    "python"
} else if `command -v python3` != "" {
    "python3"
} else {
    "python"
}

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
