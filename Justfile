# Canonical developer/agent entrypoints for the Nulang compiler/runtime.
# Keep this file thin: Cargo and scripts remain the underlying source of truth.

set shell := ["bash", "-euo", "pipefail", "-c"]

default:
    @just --list

check-nextest:
    @command -v cargo-nextest >/dev/null || { echo "cargo-nextest is required. Install it from https://nexte.st/docs/installation/pre-built-binaries/"; exit 2; }

fmt:
    cargo fmt --check

lint:
    cargo clippy --locked --all-targets -- -D clippy::correctness

test: check-nextest
    cargo nextest run --locked

test-doc:
    cargo test --locked --doc

# Fast inner-loop gate: formatting, compilation, and the default test suite.
fast-check: check-nextest
    cargo fmt --check
    cargo check --locked --tests
    cargo nextest run --locked

# Full default-feature pre-merge gate. Feature-matrix, release, WASM, formal,
# audit, and benchmark coverage remain CI-owned.
check: check-nextest
    cargo fmt --check
    cargo clippy --locked --all-targets -- -D clippy::correctness
    cargo nextest run --locked
    cargo test --locked --doc
    python3 scripts/verify_implementation.py --skip-tests

ci: check
