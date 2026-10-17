#!/usr/bin/env bash
set -euo pipefail

cargo test --locked --no-default-features --test effect_site_artifact_metadata
cargo test --locked --no-default-features --test compiled_durable_dispatch
cargo test --locked --no-default-features --test durable_effect_recovery_store
cargo test --locked --no-default-features --test durable_effect_failure_matrix
cargo test --locked --no-default-features durable_effect

cargo test --locked --no-default-features --features sqlite --test durable_effect_process_kill
