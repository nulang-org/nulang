#!/usr/bin/env bash
# Deterministic smoke test for examples/18_inventory_reservations.nula.
# Run from any directory:
#   NULANG_BIN=/absolute/path/to/nulang bash scripts/verify_inventory_example.sh
set -euo pipefail

root="$(cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd)"
cd "$root"

if [[ -n "${NULANG_BIN:-}" ]]; then
  case "$NULANG_BIN" in
    /*) binary="$NULANG_BIN" ;;
    *) binary="$root/${NULANG_BIN#./}" ;;
  esac
else
  # The repository's .cargo/config.toml selects a target directory outside
  # the checkout, so force a known location for this standalone smoke test.
  CARGO_TARGET_DIR="$root/target" cargo build --locked --bin nulang
  binary="$root/target/debug/nulang"
fi

actual="$("$binary" examples/18_inventory_reservations.nula)"
expected="$(cat <<'EXPECTED'
reservation accepted: 3
reservation rejected: 4 (insufficient stock)
available=2 reserved=3
release accepted: 1
available=3 reserved=2
EXPECTED
)"

# Only one actor receives these messages from one sender. Preserve the
# output sequence: reject over-reservation and never corrupt stock counters.
if [[ "$actual" != "$expected" ]]; then
  printf 'Inventory actor output mismatch.\nExpected:\n%s\nActual:\n%s\n' "$expected" "$actual" >&2
  exit 1
fi

echo "Inventory actor smoke test passed."
