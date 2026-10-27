#!/usr/bin/env bash
# No Turso service/credentials: only synthetic 127.0.0.1 protocol fixtures.
set -euo pipefail
root="$(git rev-parse --show-toplevel)"
cd "$root"
actual="$(git rev-parse HEAD)"
if [[ -n "${CI_COMMIT_SHA:-}" && "$CI_COMMIT_SHA" != "$actual" ]]; then
  echo "FAIL: expected Woodpecker SHA differs from actual checkout" >&2
  exit 1
fi
echo "TURSO_REMOTE_WIRE_HEAD=$actual"

# Tests are feature-gated and otherwise omitted from default/full.
cargo fmt --all -- --check
cargo metadata --locked --no-deps --format-version 1 >/dev/null
cargo test --locked --no-default-features --features turso-remote-probe --test turso_remote_preflight
cargo test --locked --no-default-features --features turso-remote-probe --test turso_remote_atomic_wire
cargo clippy --locked --no-default-features --features turso-remote-probe --test turso_remote_atomic_wire -- -D clippy::correctness
git diff --check
echo "PASS remote Turso synthetic wire contract $actual"
