#!/usr/bin/env bash
# Run Cargo with build outputs and compiler scratch files on the checkout's disk.
# Useful when /tmp is tmpfs or a small quota-limited filesystem.
set -euo pipefail
repo_root="$(cd -- "$(dirname -- "${BASH_SOURCE[0]}")/.." && pwd -P)"
mkdir -p -- "$repo_root/.build-tmp"
export TMPDIR="$repo_root/.build-tmp"
cd -- "$repo_root"
exec cargo "$@"
