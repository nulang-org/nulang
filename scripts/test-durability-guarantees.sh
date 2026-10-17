#!/usr/bin/env bash
set -euo pipefail

cargo test --locked --no-default-features --test effect_site_artifact_metadata
cargo test --locked --no-default-features --test durable_effect_recovery_store
cargo test --locked --no-default-features --test durable_effect_failure_matrix
cargo test --locked --no-default-features durable_effect
# Real subprocess SIGKILL + SQLite recovery (the test binary is Unix-only).
if [[ "$(uname -s)" != "MINGW"* && "$(uname -s)" != "MSYS"* && "$(uname -s)" != "CYGWIN"* ]]; then
  cargo test --locked --no-default-features --features sqlite --test durable_effect_process_kill_sqlite
fi
