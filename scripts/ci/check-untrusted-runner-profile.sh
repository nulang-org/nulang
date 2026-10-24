#!/usr/bin/env bash
# Exercise the production bytecode runner feature contract on the exact source.
set -euo pipefail
cd "$(dirname "$0")/../.."

# Fail closed when a new Cargo feature is added without being explicitly
# classified by the untrusted-runner compile-time guard.
python3 scripts/ci/test-untrusted-runner-feature-coverage.py

cargo check --locked -p nulang --lib --no-default-features --features untrusted-runner

# A hosted runner must not accidentally compile in a native-escape surface.
for unsafe_feature in ffi tcp python tls; do
  log="$(mktemp)"
  if cargo check --locked -p nulang --lib --no-default-features \
    --features "untrusted-runner,${unsafe_feature}" >"$log" 2>&1; then
    cat "$log"
    rm -f "$log"
    echo "FAIL: untrusted-runner unexpectedly admitted ${unsafe_feature}" >&2
    exit 1
  fi
  if ! grep -Fq 'untrusted-runner forbids host-integrating features' "$log"; then
    cat "$log"
    rm -f "$log"
    echo "FAIL: ${unsafe_feature} rejected for an unrelated build error" >&2
    exit 1
  fi
  rm -f "$log"
done
