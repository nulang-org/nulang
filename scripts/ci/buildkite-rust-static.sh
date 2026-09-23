#!/usr/bin/env bash
set -euo pipefail

cd "${BUILDKITE_BUILD_CHECKOUT_PATH:-$(pwd)}"

export DEBIAN_FRONTEND=noninteractive
apt-get update
apt-get install -y --no-install-recommends python3 python3-dev ca-certificates pkg-config
rm -rf /var/lib/apt/lists/*

rustc --version
cargo --version
rustup component add rustfmt clippy

# Match the repository's canonical CI semantics rather than inventing a stricter
# lint policy that would make Buildkite disagree with GitHub Actions.
cargo fmt --all -- --check

cargo check --locked --all-targets --no-default-features
cargo clippy --locked --all-targets --no-default-features -- -D clippy::correctness

bash scripts/check_standard_runtime_profile.sh

cargo check --locked --all-targets
cargo clippy --locked --all-targets -- -D clippy::correctness
cargo check --locked --tests

cargo check --locked --all-targets --all-features
cargo clippy --locked --all-targets --all-features -- -D clippy::correctness

# Repository-specific forbidden-pattern and zero-warning gate.
python3 scripts/verify_implementation.py

cargo doc --locked --no-deps

git diff --check
