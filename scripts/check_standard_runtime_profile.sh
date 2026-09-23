#!/usr/bin/env bash
set -euo pipefail

FEATURES="standard-runtime"

echo "==> Checking standard standalone runtime profile"
cargo check --all-targets --no-default-features --features "$FEATURES"

echo "==> Running standard runtime tests"
cargo test --no-default-features --features "$FEATURES"

echo "==> Auditing standard runtime dependency graph"
tree="$(cargo tree -p nulang --no-default-features --features "$FEATURES" --edges normal,build)"
printf '%s\n' "$tree"

# These dependency families belong to explicit opt-in surfaces, not the
# ordinary standalone runtime. Keep this list semantic rather than exhaustive:
# transitive dependencies of the allowed JIT/network stack may change.
forbidden='(^|[[:space:]├└│─])(pyo3|libsql|tower-lsp|nulang-ai([_-][[:alnum:]-]+)?|libloading|libffi|wasmtime|rocksdb|postgres|opentelemetry([_-][[:alnum:]-]+)?)[[:space:]]v'
if printf '%s\n' "$tree" | grep -Eiq "$forbidden"; then
  echo "ERROR: standard-runtime pulled an opt-in dependency family" >&2
  printf '%s\n' "$tree" | grep -Ei "$forbidden" >&2 || true
  exit 1
fi

metadata="$(cargo metadata --format-version 1 --no-deps)"
python3 - "$metadata" <<'PY'
import json
import sys

metadata = json.loads(sys.argv[1])
root = next(p for p in metadata["packages"] if p["name"] == "nulang")
features = root["features"]
expected = {"native-codegen", "tls", "tcp", "ureq"}
actual = set(features.get("standard-runtime", []))
if actual != expected:
    raise SystemExit(
        "standard-runtime feature ownership drifted: expected "
        + ", ".join(sorted(expected))
        + "; got "
        + ", ".join(sorted(actual))
    )
print("standard-runtime feature ownership: OK")
PY

echo "standard-runtime profile: OK"
