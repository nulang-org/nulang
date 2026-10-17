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

# Canonical local pre-merge gate. Same implementation as mise run check.
# Includes compiler/runtime feature matrices; hosted platform gates stay in CI.
check:
    bash scripts/ci-local.sh

# Extended local validation (release, rustdoc, audit, doc snippets, Lean).
# This does not substitute for hosted cross-platform packaging/bench/fuzz jobs.
check-full:
    bash scripts/ci-local.sh --full

ci: check
