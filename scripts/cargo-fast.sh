#!/usr/bin/env bash
set -euo pipefail

# Fast local Cargo wrapper for Nulang.
#
# - Uses mold when available, otherwise LLD when available, otherwise the
#   repository's portable system linker configuration.
# - Preserves --export-dynamic, which Nulang needs on Linux.
# - Defaults to the minimal library check when no Cargo arguments are supplied.
#
# Override linker selection with:
#   NULANG_FAST_LINKER=system|mold|lld ./scripts/cargo-fast.sh ...

if [[ $# -eq 0 ]]; then
  set -- check --lib --no-default-features
fi

linker="${NULANG_FAST_LINKER:-auto}"

if [[ "$linker" == "auto" ]]; then
  if command -v mold >/dev/null 2>&1; then
    linker="mold"
  elif command -v ld.lld >/dev/null 2>&1 || command -v lld >/dev/null 2>&1; then
    linker="lld"
  else
    linker="system"
  fi
fi

case "$linker" in
  mold)
    if ! command -v mold >/dev/null 2>&1; then
      echo "error: NULANG_FAST_LINKER=mold requested but mold is not installed" >&2
      exit 2
    fi
    export RUSTFLAGS="${RUSTFLAGS:-} -C link-arg=-Wl,--export-dynamic -C link-arg=-fuse-ld=mold"
    ;;
  lld)
    if ! command -v ld.lld >/dev/null 2>&1 && ! command -v lld >/dev/null 2>&1; then
      echo "error: NULANG_FAST_LINKER=lld requested but LLD is not installed" >&2
      exit 2
    fi
    export RUSTFLAGS="${RUSTFLAGS:-} -C link-arg=-Wl,--export-dynamic -C link-arg=-fuse-ld=lld"
    ;;
  system)
    ;;
  *)
    echo "error: NULANG_FAST_LINKER must be auto, system, mold, or lld" >&2
    exit 2
    ;;
esac

export CARGO_INCREMENTAL="${CARGO_INCREMENTAL:-1}"

echo "==> cargo linker: $linker" >&2
exec cargo "$@"
