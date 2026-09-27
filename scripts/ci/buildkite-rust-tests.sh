#!/usr/bin/env bash
set -euo pipefail

cd "${BUILDKITE_BUILD_CHECKOUT_PATH:-$(pwd)}"

export DEBIAN_FRONTEND=noninteractive
apt-get update
apt-get install -y --no-install-recommends python3 python3-dev ca-certificates pkg-config
rm -rf /var/lib/apt/lists/*

rustc --version
cargo --version

# Run the broad feature matrix here; the canonical default suite retains its
# one retry for the known timing-sensitive split-brain cluster test.
cargo test --locked --workspace || (
  echo 'Retrying default workspace suite after possible timing-sensitive failure...'
  cargo test --locked --workspace
)

cargo test --locked --workspace --no-default-features
cargo test --locked --workspace --features wasm-backend
cargo test --locked --workspace --all-targets --all-features
cargo test --locked --doc
