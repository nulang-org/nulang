#!/usr/bin/env bash
# Two independently linked SQLite executables; never combine libSQL core
# and rusqlite into one process (their SQLite C runtimes can conflict).
set -euo pipefail
root="$(git rev-parse --show-toplevel)"
cd "$root"
head="$(git rev-parse HEAD)"
if [[ -n "${CI_COMMIT_SHA:-}" && "$CI_COMMIT_SHA" != "$head" ]]; then
  echo "FAIL: stale checkout for rusqlite compatibility" >&2
  exit 1
fi
base="$root/tools/rusqlite-local-compat"
dir="$(mktemp -d "${TMPDIR:-/tmp}/nulang-rusqlite-parity.XXXXXXXX")"
trap 'rm -rf -- "$dir"' EXIT
export NULANG_LEGACY_FIXTURE_DB="$dir/legacy.db"
export RUSTC_WRAPPER='' CARGO_INCREMENTAL=0 CARGO_BUILD_JOBS=2
export CARGO_PROFILE_DEV_DEBUG=0 CARGO_PROFILE_TEST_DEBUG=0
# Caller may override CARGO_TARGET_DIR to keep large builds off constrained /tmp.
export CARGO_TARGET_DIR="${CARGO_TARGET_DIR:-${XDG_CACHE_HOME:-$HOME/.cache}/nulang-rusqlite-compat-target}"
echo "LIBSQL_RUSQLITE_PARITY_HEAD=$head"
cargo run --locked --manifest-path "$base/writer/Cargo.toml" -- "$NULANG_LEGACY_FIXTURE_DB"
cargo test --locked --manifest-path "$base/reader/Cargo.toml" --test parity
# Experimental writer controls are exercised only in an independently
# linked rusqlite process; no root PersistenceStore backend changes.
cargo test --locked --manifest-path "$base/reader/Cargo.toml" --test atomic_writer
# Fail-closed typed recovery of versioned durable external effects and
# staged outbox messages without dispatching or acknowledging delivery.
cargo test --locked --manifest-path "$base/reader/Cargo.toml" --test effect_outbox
cargo clippy --locked --manifest-path "$base/writer/Cargo.toml" -- -D clippy::correctness
cargo clippy --locked --manifest-path "$base/reader/Cargo.toml" --all-targets -- -D clippy::correctness
echo "PASS separate-process SQLite local recovery and experimental atomic writer for $head"
