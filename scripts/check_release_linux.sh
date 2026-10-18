#!/usr/bin/env bash
# Smoke a Linux release-profile CLI independently of the build environment's
# Python installation. A previously released full-feature binary required
# libpython3.12.so.1.0 and failed on hosts with only another Python ABI.
set -euo pipefail

if [ "$#" -ne 1 ]; then
  echo "usage: $0 <linux-nulang-binary>" >&2
  exit 2
fi

binary="$1"
if [ ! -f "$binary" ] || [ ! -x "$binary" ]; then
  echo "not an executable file: $binary" >&2
  exit 2
fi

command -v readelf >/dev/null || { echo "readelf is required" >&2; exit 2; }

# Failed inspections must never be interpreted as a clean dependency report.
# Capture the complete command output before testing it: grep -q in a pipe
# can stop reading early and produce a misleading SIGPIPE under pipefail.
if ! dynamic_section="$(LC_ALL=C readelf -d "$binary")"; then
  echo "readelf could not inspect release CLI dynamic dependencies" >&2
  exit 1
fi
if grep -Eiq 'Shared library: \[libpython[^]]*\]' <<<"$dynamic_section"; then
  echo "release CLI must not require a version-specific libpython shared library" >&2
  exit 1
fi

if ! program_headers="$(LC_ALL=C readelf -l "$binary")"; then
  echo "readelf could not inspect release CLI program headers" >&2
  exit 1
fi

# A static ELF has no dynamic program interpreter; ldd may legitimately exit
# nonzero with "not a dynamic executable". Only invoke it for dynamic ELFs.
if grep -Fq 'Requesting program interpreter:' <<<"$program_headers"; then
  command -v ldd >/dev/null || { echo "ldd is required" >&2; exit 2; }
  if ! linked_libraries="$(LC_ALL=C ldd "$binary" 2>&1)"; then
    echo "ldd could not inspect release CLI dependencies" >&2
    exit 1
  fi
  if grep -Fq 'not found' <<<"$linked_libraries"; then
    echo "release CLI has unresolved shared library dependencies:" >&2
    printf '%s\n' "$linked_libraries" >&2
    exit 1
  fi
fi

"$binary" --version >/dev/null
output="$("$binary" --eval 'perform IO.print("release-smoke")')"
if ! grep -Fxq 'release-smoke' <<<"$output"; then
  echo "release eval smoke produced unexpected output: $output" >&2
  exit 1
fi
echo "PASS: Linux release CLI links no libpython and executes --version / --eval"
