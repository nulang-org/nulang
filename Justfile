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

# Faster, opt-in compiler/backend edit loops. Default/release builds retain
# their existing feature and debug contracts.
check-core:
    cargo check --locked --no-default-features --lib --profile dev-fast

check-jit:
    cargo check --locked --no-default-features --features native-codegen --lib --profile dev-fast

check-wasm:
    cargo check --locked --no-default-features --features wasm-backend --lib --profile dev-fast

build-core:
    cargo build --locked --no-default-features --bin nulang --profile dev-fast

# Cargo writes HTML timing reports into the chosen target profile directory.
build-timings:
    cargo build --locked --timings --profile dev-fast

build-timings-core:
    cargo build --locked --timings --no-default-features --bin nulang --profile dev-fast

# Optional Linux linker experiment. RUSTFLAGS replaces target rustflags, so
# repeat export-dynamic to preserve the native FFI linker contract.
build-mold:
    @command -v mold >/dev/null || { echo "mold not found; install mold first" >&2; exit 2; }
    RUSTFLAGS="${RUSTFLAGS:+${RUSTFLAGS} }-C link-arg=-fuse-ld=mold -C link-arg=-Wl,--export-dynamic" cargo build --locked --profile dev-fast

# Opt-in, same-host comparison of cold and no-op builds. Creates separate
# disposable target directories; never clears the developer's Cargo cache.
benchmark-dev-profiles:
    python3 scripts/compare_build_profiles.py --output target/dev-profile-benchmarks.json

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
