#!/usr/bin/env bash
# Reproducible Linux binary-size comparison for the same stdout contract.
# Reports linkage explicitly: dynamically linked ELF files are NOT equivalent
# to portable, fully statically linked executables.
set -euo pipefail

if [[ $# -ne 2 ]]; then
  echo "usage: $0 <nulang-compiler> <new-output-directory>" >&2
  exit 2
fi
if [[ "$(uname -s)" != Linux ]]; then
  echo "error: this harness currently requires Linux" >&2
  exit 2
fi
if ! command -v cc >/dev/null || ! command -v readelf >/dev/null; then
  echo "error: cc and readelf (binutils) are required" >&2
  exit 2
fi

compiler="$1"
out="$2"
if [[ ! -f "$compiler" || ! -x "$compiler" ]]; then
  echo "error: Nulang compiler is not executable: $compiler" >&2
  exit 2
fi
if [[ -e "$out" ]]; then
  echo "error: output directory already exists (refusing overwrite): $out" >&2
  exit 2
fi
mkdir -p "$out"
out="$(realpath "$out")"
compiler="$(realpath "$compiler")"

cat > "$out/hello.nula" <<'NULA'
perform IO.print("Hello from binary-size benchmark")
NULA
cat > "$out/hello.c" <<'C'
#include <stdio.h>
int main(void) {
    puts("Hello from binary-size benchmark");
    return 0;
}
C

"$compiler" --emit-exe --out "$out/hello-nulang" "$out/hello.nula"
cc -Os -s -ffunction-sections -fdata-sections \
  -Wl,--gc-sections "$out/hello.c" -o "$out/hello-c"

if command -v go >/dev/null 2>&1 || command -v tinygo >/dev/null 2>&1; then
  cat > "$out/hello.go" <<'GO'
package main
import "fmt"
func main() { fmt.Println("Hello from binary-size benchmark") }
GO
fi
if command -v go >/dev/null 2>&1; then
  (cd "$out" && go build -trimpath -ldflags="-s -w" -o hello-go hello.go)
fi
if command -v tinygo >/dev/null 2>&1; then
  tinygo build -opt=z -no-debug -o "$out/hello-tinygo" "$out/hello.go"
fi
if command -v rustc >/dev/null 2>&1; then
  cat > "$out/hello.rs" <<'RUST'
fn main() { println!("Hello from binary-size benchmark"); }
RUST
  rustc -C opt-level=z -C strip=symbols "$out/hello.rs" -o "$out/hello-rust"
fi

printf 'Hello from binary-size benchmark\n' > "$out/expected.stdout"
report="$out/report.tsv"
printf 'language\tbytes\tlinkage\tneeded_libraries\n' > "$report"

measure() {
  local language="$1" binary="$2" needed linkage
  "$binary" > "$out/$language.stdout"
  if ! cmp -s "$out/expected.stdout" "$out/$language.stdout"; then
    echo "error: $language stdout differs from the common fixture" >&2
    exit 1
  fi
  needed="$(readelf -d "$binary" 2>/dev/null |
    sed -nE '/\(NEEDED\)/s/.*\[(.*)\].*/\1/p' |
    paste -sd, -)"
  if [[ -n "$needed" ]] || readelf -l "$binary" 2>/dev/null | grep -q 'Requesting program interpreter'; then
    linkage="dynamic"
  else
    linkage="no-dynamic-loader-detected"
  fi
  printf '%s\t%s\t%s\t%s\n' "$language" "$(stat -c %s "$binary")" "$linkage" "${needed:-none}" >> "$report"
}

measure nulang "$out/hello-nulang"
measure c "$out/hello-c"
[[ ! -x "$out/hello-go" ]] || measure go "$out/hello-go"
[[ ! -x "$out/hello-tinygo" ]] || measure tinygo "$out/hello-tinygo"
[[ ! -x "$out/hello-rust" ]] || measure rust "$out/hello-rust"
cat "$report"
echo "Measurements and compiled artifacts: $out"
echo "Note: size alone is not a fair cross-language ranking; inspect linkage and runtime semantics."
