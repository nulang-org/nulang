#!/usr/bin/env bash
set -euo pipefail

DAGGER_VERSION="1.0.0-beta.14"
ROOT="$(cd "$(dirname "${BASH_SOURCE[0]}")/../.." && pwd)"
BIN_DIR="${DAGGER_BIN_DIR:-${XDG_CACHE_HOME:-$HOME/.cache}/nulang/dagger/${DAGGER_VERSION}}"
DAGGER_BIN="${BIN_DIR}/dagger"

if [[ ! -x "$DAGGER_BIN" ]]; then
  mkdir -p "$BIN_DIR"
  curl -fsSL https://dl.dagger.io/dagger/install.sh |
    DAGGER_VERSION="$DAGGER_VERSION" BIN_DIR="$BIN_DIR" sh
fi

cd "$ROOT"
exec "$DAGGER_BIN" --no-mod --progress=plain ci/dagger/core.dag
