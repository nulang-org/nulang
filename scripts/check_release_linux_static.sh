#!/usr/bin/env bash
# Reject ELF binaries that require a dynamic loader or shared libraries.
# This checks deployment linkage, not whether optional runtime dlopen calls work.
set -euo pipefail

if [ "$#" -ne 1 ]; then
  echo "usage: $0 <linux-elf-binary>" >&2
  exit 2
fi
binary="$1"
if [ ! -f "$binary" ] || [ ! -x "$binary" ]; then
  echo "missing or non-executable binary: $binary" >&2
  exit 1
fi
command -v readelf >/dev/null || { echo "readelf is required" >&2; exit 2; }
if ! readelf -h "$binary" >/dev/null 2>&1; then
  echo "not an ELF executable: $binary" >&2
  exit 1
fi
if ! program_headers="$(readelf -lW "$binary")"; then
  echo "unable to inspect ELF program headers: $binary" >&2
  exit 1
fi
if grep -Eq '^[[:space:]]*INTERP[[:space:]]' <<<"$program_headers"; then
  echo "binary requires an ELF interpreter (dynamic loader): $binary" >&2
  exit 1
fi
if ! dynamic_headers="$(readelf -dW "$binary")"; then
  echo "unable to inspect ELF dynamic section: $binary" >&2
  exit 1
fi
if grep -Eq '\((NEEDED|RPATH|RUNPATH)\)' <<<"$dynamic_headers"; then
  echo "binary declares shared-library or search-path dependencies: $binary" >&2
  exit 1
fi
echo "PASS: $binary has no ELF interpreter or dynamic shared-library dependencies"
