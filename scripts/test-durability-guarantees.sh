#!/usr/bin/env bash
set -euo pipefail

# Keep the minimal and SQLite-backed profiles separate: a test gated with
# #![cfg(feature = "sqlite")] otherwise passes with zero tests when run with
# --no-default-features alone.
for suite in \
  effect_site_artifact_metadata \
  compiled_durable_dispatch \
  authorized_compiled_durable_dispatch \
  durable_effect_recovery_store \
  durable_effect_failure_matrix \
  durable_activation_epoch \
  nudb_crash_recovery \
  workflow_safe_snapshot \
  workflow_timer_replay; do
  cargo test --locked --no-default-features --test "$suite"
done

for suite in \
  workflow_effect_tail_recovery \
  workflow_timer_process_restart_replay; do
  cargo test --locked --no-default-features --features sqlite --test "$suite"
done

cargo test --locked --no-default-features durable_effect
