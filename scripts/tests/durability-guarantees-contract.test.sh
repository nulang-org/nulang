#!/usr/bin/env bash
set -euo pipefail

ROOT="$(cd "$(dirname "${BASH_SOURCE[0]}")/../.." && pwd)"
TMP="$(mktemp -d)"
trap 'rm -rf "$TMP"' EXIT

cat > "$TMP/cargo" <<'MOCK'
#!/usr/bin/env bash
printf '%s\n' "$*" >> "$CAPTURE_CALLS"
if [[ "${FAIL_ON:-}" != "" && "$*" == *"$FAIL_ON"* ]]; then
  exit 23
fi
MOCK
chmod +x "$TMP/cargo"
export PATH="$TMP:$PATH" CAPTURE_CALLS="$TMP/calls"

bash "$ROOT/scripts/test-durability-guarantees.sh"
grep -Fqx 'test --locked --no-default-features --features sqlite --test workflow_effect_tail_recovery' "$CAPTURE_CALLS"
grep -Fqx 'test --locked --no-default-features --features sqlite --test workflow_timer_process_restart_replay' "$CAPTURE_CALLS"
grep -Fqx 'test --locked --no-default-features --test nudb_crash_recovery' "$CAPTURE_CALLS"
if grep -F -- '--no-default-features --test workflow_timer_process_restart_replay' "$CAPTURE_CALLS"; then
  echo 'SQLite-only restart tests must never run without SQLite' >&2
  exit 1
fi

: > "$CAPTURE_CALLS"
export FAIL_ON='workflow_effect_tail_recovery'
if bash "$ROOT/scripts/test-durability-guarantees.sh"; then
  echo 'A failed durability test incorrectly passed the certification suite' >&2
  exit 1
fi

echo 'Durability certification command contract passed'
