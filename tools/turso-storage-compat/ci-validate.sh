#!/usr/bin/env bash
# Exact-head manual compatibility gate. This never reads a production DB.
set -euo pipefail

root="$(git rev-parse --show-toplevel)"
cd "$root/tools/turso-storage-compat"
echo "TURSO_COMPAT_HEAD=$(git -C "$root" rev-parse HEAD)"
echo "TURSO_COMPAT_WORKSPACE=$PWD"

# A stale checkout must not be reported as evidence for a newer commit.
# Woodpecker injects CI_COMMIT_SHA for PR/push/manual jobs.
actual_head="$(git -C "$root" rev-parse HEAD)"
if [[ -n "${CI_COMMIT_SHA:-}" && "${CI_COMMIT_SHA}" != "$actual_head" ]]; then
  echo "FAIL Turso CI expected commit ${CI_COMMIT_SHA}, actual $actual_head" >&2
  exit 1
fi

# Fail if this ever ceases to be an isolated Cargo workspace.
test -s Cargo.lock
grep -qx '\[workspace\]' Cargo.toml
grep -q '^turso = { version = "=0.8.3"' Cargo.toml

cargo fmt --all -- --check
cargo metadata --locked --no-deps --format-version 1 >/dev/null
cargo clippy --locked -- -D warnings

# No network/database environment variables are needed by this disposable,
# local-file test. Real subprocess writers and SIGKILL run in this container.
cargo run --locked

echo "PASS standalone Turso local-recovery contract at $actual_head"
