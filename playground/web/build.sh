#!/usr/bin/env sh
# Build the browser playground bundle.
#
# Produces playground/web/nulang_playground.wasm next to the static files
# (index.html, playground.js, style.css). The .wasm is a build artifact and
# is intentionally NOT committed to git.
#
# Requires: Rust toolchain with the wasm32-unknown-unknown target:
#   rustup target add wasm32-unknown-unknown
set -eu

ROOT="$(cd "$(dirname "$0")/../.." && pwd)"
cd "$ROOT/crates/nulang-playground"

cargo build --release --target wasm32-unknown-unknown

# Resolve the effective target directory (the repo pins CARGO_TARGET_DIR via
# .cargo/config.toml, so it is not always ./target).
TARGET_DIR="$(cargo metadata --no-deps --format-version 1 | sed -n 's/.*"target_directory":"\([^"]*\)".*/\1/p')"
WASM="$TARGET_DIR/wasm32-unknown-unknown/release/nulang_playground.wasm"
[ -f "$WASM" ] || { echo "error: $WASM not found" >&2; exit 1; }

cp "$WASM" "$ROOT/playground/web/nulang_playground.wasm"
echo "wrote playground/web/nulang_playground.wasm ($(wc -c < "$ROOT/playground/web/nulang_playground.wasm") bytes)"
echo "serve with:  cd playground/web && python3 -m http.server 8080"
