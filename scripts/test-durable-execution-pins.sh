#!/usr/bin/env bash
set -euo pipefail

cd "$(dirname "$0")/.."
cargo test --locked --test durable_execution_pin
cargo test --locked --bin nulang-execution-preflight
