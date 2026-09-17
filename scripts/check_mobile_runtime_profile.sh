#!/usr/bin/env bash
set -euo pipefail

FEATURES="mobile-runtime"

echo "==> Checking interpreter-only mobile runtime"
cargo check --lib --no-default-features --features "$FEATURES"

echo "==> Running library tests for mobile runtime profile"
cargo test --lib --no-default-features --features "$FEATURES"

echo "==> Auditing runtime dependency graph"
tree="$(cargo tree --no-default-features --features "$FEATURES" --edges normal,build)"
printf '%s\n' "$tree"

forbidden='(^|[[:space:]├└│─])(cranelift([[:alnum:]_-]*)?|wasmtime|libloading)[[:space:]]v'
if printf '%s\n' "$tree" | grep -Eiq "$forbidden"; then
  echo "ERROR: mobile-runtime dependency graph contains native codegen/dynamic-loader crates" >&2
  printf '%s\n' "$tree" | grep -Ei "$forbidden" >&2 || true
  exit 1
fi

metadata="$(cargo metadata --format-version 1 --no-deps)"
python3 - "$metadata" <<'PY'
import json, sys
metadata = json.loads(sys.argv[1])
root = next(p for p in metadata["packages"] if p["name"] == "nulang")
features = root["features"]
required = {
    "dep:cranelift", "dep:cranelift-jit", "dep:cranelift-module",
    "dep:cranelift-native", "dep:cranelift-frontend", "dep:cranelift-codegen",
    "dep:target-lexicon",
}
missing = sorted(required - set(features.get("native-codegen", [])))
if missing:
    raise SystemExit("native-codegen missing dependency ownership: " + ", ".join(missing))
mobile = set(features.get("mobile-runtime", []))
forbidden = sorted(x for x in mobile if x == "native-codegen" or x == "ffi" or x.startswith("dep:cranelift") or x in {"dep:wasmtime", "dep:libloading"})
if forbidden:
    raise SystemExit("mobile-runtime enables forbidden features: " + ", ".join(forbidden))
if "native-codegen" not in set(features.get("default", [])):
    raise SystemExit("default builds must retain native-codegen")
print("mobile-runtime feature ownership: OK")
PY

echo "mobile-runtime profile: OK"
