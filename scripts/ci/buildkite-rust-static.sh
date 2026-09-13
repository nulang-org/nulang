#!/usr/bin/env bash
set -euo pipefail

cd "${BUILDKITE_BUILD_CHECKOUT_PATH:-$(pwd)}"

rustc --version
cargo --version
rustup component add rustfmt clippy

cargo fmt --all -- --check
cargo check --locked --all-targets --no-default-features
cargo check --locked --all-targets
cargo check --locked --all-targets --all-features
cargo clippy --locked --all-targets --all-features -- -D warnings

git diff --check
