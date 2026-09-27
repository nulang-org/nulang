#!/usr/bin/env bash
set -euo pipefail

fail() {
  printf 'nulang Dagger/Woodpecker contract: %s\n' "$*" >&2
  exit 1
}

require_file() {
  [[ -f "$1" ]] || fail "missing required file: $1"
}

require_literal() {
  grep -Fq -- "$2" "$1" || fail "$1 is missing required contract: $2"
}

woodpecker=".woodpecker/ci.yml"
smoke=".woodpecker/dagger-smoke.yml"
dagger="ci/dagger/core.dag"
runner="scripts/ci/run-dagger-core.sh"

for file in "$woodpecker" "$smoke" "$dagger" "$runner"; do
  require_file "$file"
done

# The portable and Woodpecker paths must share the existing repository-owned
# static/test contracts instead of reimplementing them in CI YAML.
for script in scripts/ci/buildkite-rust-static.sh scripts/ci/buildkite-rust-tests.sh; do
  require_literal "$woodpecker" "bash $script"
  require_literal "$dagger" "bash $script"
done

require_literal "$runner" 'DAGGER_VERSION="1.0.0-beta.14"'
require_literal "$smoke" 'event: manual'
require_literal "$smoke" 'branch: main'
require_literal "$smoke" 'NULANG_VERIFICATION_LANE == "dagger-smoke"'
require_literal "$smoke" 'privileged: true'
require_literal "$woodpecker" 'NULANG_VERIFICATION_LANE == "ci" || NULANG_VERIFICATION_LANE == "all"'
require_literal "$smoke" 'podman/podman.sock:/var/run/docker.sock'
require_literal "$smoke" 'bash scripts/ci/run-dagger-core.sh'

if grep -Fq 'event: pull_request' "$smoke"; then
  fail "Dagger socket-bearing smoke workflow must never run on pull requests"
fi

printf 'Nulang Dagger/Woodpecker contract OK\n'
