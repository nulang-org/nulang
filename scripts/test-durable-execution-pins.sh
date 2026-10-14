#!/usr/bin/env bash
set -euo pipefail

cd "$(dirname "$0")/.."
cargo test --locked --test durable_execution_pin
cargo test --locked --bin nulang-execution-preflight
cargo test --locked --test pinned_workflow_atomic_start
cargo test --locked --test pinned_workflow_runtime
cargo test --locked --features sqlite --test pinned_workflow_atomic_start
cargo test --locked --features sqlite --test pinned_workflow_runtime
cargo test --locked --test pinned_bytecode_loader
cargo test --locked --features sqlite --test pinned_bytecode_loader
