#!/usr/bin/env bash
set -euo pipefail
ROOT="$(cd "$(dirname "${BASH_SOURCE[0]}")/../.." && pwd)"
cd "$ROOT"

bash -n scripts/dev-doctor.sh
help="$(bash scripts/dev-doctor.sh --help)"
grep -q -- '--profile core' <<<"$help"
grep -q -- '--profile full' <<<"$help"

# The core compiler profile must work without the optional AI/build-farm tooling.
output="$(bash scripts/dev-doctor.sh --profile core 2>&1)" || {
  printf '%s\n' "$output" >&2
  exit 1
}
grep -q 'doctor passed' <<<"$output"
! grep -Eq 'missing required command: (mbx|mise|python3|pkg-config)' <<<"$output"

if bash scripts/dev-doctor.sh --profile unsupported >/dev/null 2>&1; then
  echo "invalid profile was accepted" >&2
  exit 1
fi
echo 'PASS: core and full Linux developer profiles'
