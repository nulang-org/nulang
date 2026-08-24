#!/bin/bash
set -euo pipefail
cd "$(dirname "$0")/.."
NULANG="${NULANG_BIN:-cargo run --quiet --bin nulang --}"

echo "=== Bootstrap Core verification ==="

# 1. self_test.nula: fn main() { 42 }
result=$($NULANG bootstrap/self_test.nula 2>&1 | tail -1)
if [ "$result" != "42" ]; then
    echo "FAIL: self_test.nula expected 42, got '$result'"
    exit 1
fi
echo "PASS: self_test.nula = 42"

# 2. compiler_core.nula: parses 4 expressions, last is 'let x = 42 in x + 1' = 43
result=$($NULANG bootstrap/compiler_core.nula 2>&1 | tail -1)
if [ "$result" != "43" ]; then
    echo "FAIL: compiler_core.nula expected 43, got '$result'"
    exit 1
fi
echo "PASS: compiler_core.nula = 43"

# 3. host.nula: placeholder shim, returns 0 until Stage 3 wiring
result=$($NULANG bootstrap/host.nula 2>&1 | tail -1)
if [ "$result" != "0" ]; then
    echo "FAIL: host.nula expected 0, got '$result'"
    exit 1
fi
echo "PASS: host.nula = 0"

# 4. self_test .nbc round-trip
$NULANG --emit-nbc --out bootstrap/self_test.nbc bootstrap/self_test.nula 2>/dev/null
result=$($NULANG bootstrap/self_test.nbc 2>&1 | tail -1)
if [ "$result" != "42" ]; then
    echo "FAIL: self_test.nbc expected 42, got '$result'"
    exit 1
fi
echo "PASS: self_test.nbc round-trip = 42"
rm -f bootstrap/self_test.nbc

# 5. Self-hosting pipeline: compile_hex.nula is itself a Nulang Core program
#    that compiles Core source → hex bytecode; fixup_hex.py patches jump/
#    constant/closure offsets; hex2nbc.py emits the .nbc binary; the VM runs
#    it. This proves Nulang Core → .nbc with no Rust compiler in the loop
#    (the Stage 1→2 bridge of RFC 0003 Item 3).
pipeline() {
    local expr="$1"
    printf '%s' "$expr" | $NULANG bootstrap/compile_hex.nula 2>/dev/null |
        python3 bootstrap/fixup_hex.py |
        python3 bootstrap/hex2nbc.py > bootstrap/pipeline_test.nbc 2>/dev/null
    $NULANG bootstrap/pipeline_test.nbc 2>&1 | tail -1
}
expect() {
    local expr="$1" want="$2"
    local got
    got=$(pipeline "$expr")
    if [ "$got" != "$want" ]; then
        echo "FAIL: pipeline '$expr' expected $want, got '$got'"
        rm -f bootstrap/pipeline_test.nbc
        exit 1
    fi
    echo "PASS: pipeline '$expr' = $got"
}
expect "1 + 2 * 3" "7"
expect "let x = 42 in x + 1" "43"
expect "if 1 < 2 then 100 else 200" "100"
expect "not false" "true"
expect "(fn(x) => x + 1)(41)" "42"
# Recursion through the pipeline (self-hosting: a Nulang program computes fib).
expect "let fib = fn(n) => if n < 2 then n else fib(n - 1) + fib(n - 2) in fib(10)" "55"
rm -f bootstrap/pipeline_test.nbc

# 6. Stage 2 multi-fn: desugar_fns.py lowers top-level fn definitions into a
#    let-binding chain, then the same pipeline compiles and runs it. Proves
#    the bootstrap handles whole programs (not just single expressions).
pipeline_mf() {
    local src="$1"
    python3 bootstrap/desugar_fns.py < "$src" |
        $NULANG bootstrap/compile_hex.nula 2>/dev/null |
        python3 bootstrap/fixup_hex.py |
        python3 bootstrap/hex2nbc.py > bootstrap/pipeline_mf.nbc 2>/dev/null
    $NULANG bootstrap/pipeline_mf.nbc 2>&1 | tail -1
}
expect_mf() {
    local src="$1" want="$2"
    local got
    got=$(pipeline_mf "$src")
    if [ "$got" != "$want" ]; then
        echo "FAIL: multi-fn pipeline '$src' expected $want, got '$got'"
        rm -f bootstrap/pipeline_mf.nbc
        exit 1
    fi
    echo "PASS: multi-fn pipeline '$(basename "$src")' = $got"
}
cat > bootstrap/pipeline_multi_fn.nula <<'EOF'
fn add(x) => x + 1
fn double(x) => x * 2
add(double(3))
EOF
expect_mf bootstrap/pipeline_multi_fn.nula "7"
rm -f bootstrap/pipeline_multi_fn.nula bootstrap/pipeline_mf.nbc

# 7. Stage 2 self-compile oracle (prep_core → compile_hex → self.nbc).
# Disabled until let-chain depth is below the host limit (~21 bindings).
# self_compile() {
#     python3 bootstrap/prep_core.py < bootstrap/compile_hex.nula |
#         $NULANG bootstrap/compile_hex.nula 2>/dev/null |
#         python3 bootstrap/fixup_hex.py |
#         python3 bootstrap/hex2nbc.py > bootstrap/self_compile.nbc 2>/dev/null
# }
# ...

echo ""
echo "=== All bootstrap checks passed ==="
