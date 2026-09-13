#!/usr/bin/env bash
set -euo pipefail

cd "${BUILDKITE_BUILD_CHECKOUT_PATH:-$(pwd)}"

rustc --version
cargo --version

cargo test --locked --workspace --all-targets --all-features
cargo test --locked --doc
