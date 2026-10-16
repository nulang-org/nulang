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
command -v ldd >/dev/null || { echo "ldd is required" >&2; exit 2; }

# Fail even on a builder with Python installed: the artifact must not silently
# tie the public CLI to the builder's exact libpython SONAME.
if readelf -d "$binary" | grep -Eiq 'Shared library: \[libpython[^]]*\]'; then
  echo "release CLI must not require a version-specific libpython shared library" >&2
  exit 1
fi

if ldd "$binary" | grep -q 'not found'; then
  echo "release CLI has unresolved shared library dependencies:" >&2
  ldd "$binary" >&2
  exit 1
fi

"$binary" --version >/dev/null
output="$("$binary" --eval 'perform IO.print("release-smoke")')"
if ! grep -Fxq 'release-smoke' <<<"$output"; then
  echo "release eval smoke produced unexpected output: $output" >&2
  exit 1
fi
echo "PASS: Linux release CLI links no libpython and executes --version / --eval"
