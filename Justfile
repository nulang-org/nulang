# Canonical developer/agent entrypoints for the Nulang compiler/runtime.
# Keep this file thin: Cargo and scripts remain the underlying source of truth.

set shell := ["bash", "-euo", "pipefail", "-c"]

default:
    @just --list

check-nextest:
    @command -v cargo-nextest >/dev/null || { echo "cargo-nextest is required. Install it from https://nexte.st/docs/installation/pre-built-binaries/"; exit 2; }

check-hyperfine:
    @command -v hyperfine >/dev/null || { echo "hyperfine is required for wall-clock benchmarking. Install it from https://github.com/sharkdp/hyperfine"; exit 2; }

fmt:
    cargo fmt --check

lint:
    cargo clippy --locked --all-targets -- -D clippy::correctness

test: check-nextest
    cargo nextest run --locked

test-doc:
    cargo test --locked --doc

# Process-level wall-clock benchmark complements the runner's internal
# message throughput measurements. Compilation is outside the timed region.
bench-actor-wall: check-hyperfine
    cargo build --locked --profile savina --no-default-features --features savina-bench --bin nulang-savina
    @binary="${CARGO_TARGET_DIR:-/tmp/cargo-target}/savina/nulang-savina"; test -x "$binary" || { echo "Missing benchmark binary: $binary" >&2; exit 2; }; hyperfine --warmup 2 --runs 10 --export-json "${HYPERFINE_OUTPUT:-/tmp/nulang-actor-wall.json}" "$binary --benchmark ping_pong --repeat 1 --format jsonl"

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
