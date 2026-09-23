#!/usr/bin/env bash
set -euo pipefail

# Reproduce the blocking CI contract locally before opening/merging a PR.
# CI remains the source of truth; this script is the reproducible local entry
# point and should fail earlier than CI for the same compiler/runtime drift.
#
# Usage:
#   bash scripts/ci-local.sh          # core compiler/runtime matrix
#   bash scripts/ci-local.sh --full   # also release/docs/audit/Lean gates
#
# Keep this script in lockstep with .github/workflows/ci.yml. The intent is
# that humans and coding agents have one canonical definition of "green".

ROOT="$(cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd)"
cd "$ROOT"

FULL=0
case "${1:-}" in
  "") ;;
  --full) FULL=1 ;;
  *)
    echo "usage: $0 [--full]" >&2
    exit 2
    ;;
esac

export CARGO_TERM_COLOR="${CARGO_TERM_COLOR:-always}"

run() {
  printf '\n==> %s\n' "$*"
  "$@"
}

# Fastest failures first.
run cargo fmt --check
run cargo clippy --all-targets -- -D clippy::correctness
run cargo check --tests

# Default feature matrix.
run cargo build --all-targets
run cargo test
run python3 scripts/verify_implementation.py

# Minimal feature matrix. This is intentionally run without adding any Python
# system dependency here; CI uses the same property to prove optional features
# are genuinely optional.
run cargo build --all-targets --no-default-features
run cargo test --no-default-features
run cargo check --tests --no-default-features
run cargo clippy --all-targets --no-default-features -- -D clippy::correctness

# WASM/all-features paths catch backend-specific semantic drift.
run cargo build --all-targets --features wasm-backend
run cargo test --features wasm-backend
run cargo clippy --all-targets --all-features -- -D clippy::correctness
run cargo check --all-features

if [[ "$FULL" -eq 1 ]]; then
  run cargo build --release
  run cargo test --release
  run cargo doc --no-deps

  if command -v cargo-audit >/dev/null 2>&1; then
    run cargo audit
  else
    echo >&2
    echo "error: --full requires cargo-audit (install with: cargo install cargo-audit --locked)" >&2
    exit 1
  fi

  # Build the binary into the location used by the documentation verifier.
  run env CARGO_TARGET_DIR=./target cargo build --release --bin nulang
  run env NULANG_BIN=./target/release/nulang bash scripts/verify_doc_examples.sh

  if command -v lake >/dev/null 2>&1; then
    printf '\n==> Lean formalization\n'
    (
      cd spec/formal
      lake build
      count="$(grep -rc '\bsorry\b' ./*.lean | awk -F: '{sum+=$2} END {print sum+0}')"
      baseline=1
      echo "sorry count: $count (baseline: $baseline)"
      if [[ "$count" -gt "$baseline" ]]; then
        echo "error: Lean sorry count ($count) exceeds baseline ($baseline)" >&2
        exit 1
      fi
    )
  else
    echo >&2
    echo "error: --full requires Lean/lake on PATH" >&2
    exit 1
  fi
fi

printf '\nAll requested CI-local gates passed.\n'
