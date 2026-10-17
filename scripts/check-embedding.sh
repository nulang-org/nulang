#!/usr/bin/env bash
# Focused smoke for the public C ABI and the idiomatic Rust embedding wrapper.
# Requires Rust, Cargo, cc and nm on a Linux host.
set -euo pipefail

if [[ "$(uname -s)" != "Linux" ]]; then
  echo "Embedding shared-library smoke currently targets Linux only" >&2
  exit 2
fi

features=(--locked --no-default-features --features ffi)
for suite in ffi::c_api::tests ffi::native::tests ffi::embed::tests; do
  cargo test "${features[@]}" --lib "$suite"
done

cargo build "${features[@]}" --lib
# Honor .cargo/config.toml, CARGO_TARGET_DIR and any workspace override.
# Cargo's configured target directory in this repository is /tmp/cargo-target,
# not ./target; resolve it instead of assuming a local target directory.
target_dir="$(cargo metadata --locked --no-deps --format-version 1 | python3 -c 'import json,sys; print(json.load(sys.stdin)["target_directory"])')"
library="$target_dir/debug/libnulang.so"
if [[ ! -s "$library" ]]; then
  echo "Shared library was not produced at Cargo's target path: $library" >&2
  exit 1
fi

for symbol in \
  nulang_runtime_new \
  nulang_runtime_register_native_function \
  nulang_value_to_string_in_module \
  nulang_run_owned \
  nulang_call_function_owned \
  nulang_owned_value_from_module \
  nulang_owned_value_int_new \
  nulang_owned_value_int \
  nulang_owned_value_string_new \
  nulang_owned_value_to_string \
  nulang_owned_value_release; do
  if ! nm -D --defined-only "$library" | awk -v symbol="$symbol" '$NF == symbol { found=1 } END { exit !found }'; then
    echo "Required C ABI export missing: $symbol" >&2
    exit 1
  fi
done

mkdir -p "$target_dir/embed-smoke"
cc -std=c11 -Wall -Wextra -Werror \
  -Iinclude \
  examples/ffi_example.c \
  -L"$target_dir/debug" -lnulang \
  -Wl,-rpath,"$target_dir/debug" \
  -o "$target_dir/embed-smoke/ffi_example"

"$target_dir/embed-smoke/ffi_example"
