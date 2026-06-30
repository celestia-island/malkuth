# plana — generic Rust toolkit for service supervision.
# Single crate; no workspace.

set shell := ["bash", "-c"]

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

# Run clippy with -D warnings.
clippy:
    cargo clippy --all-targets --all-features -- -D warnings

# Run the test suite.
test:
    cargo test --all-features

# Build all features.
build:
    cargo build --all-features

# One-shot gate: fmt-check + clippy + test.
ci:
    just fmt-check
    just clippy
    just test
