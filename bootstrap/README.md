# Nulang Bootstrap Compiler

> Self-hosting path for Nulang: a Nulang→`.nbc` compiler written in Nulang Core
> (RFC 0002). Decouples the language from the Rust host's survival.

## Strategy (3-stage bootstrap)

### Stage 0 (Current — Rust host)
The existing Rust compiler (`nulang`) parses, type-checks, and compiles Nulang
Core programs. It serves as the **host** for Stages 1–2.

### Stage 1 (Working — Nulang Core parser, type-checker, and bytecode emitter)
`compiler_core.nula` is a Nulang Core program that parses and type-checks a
Core expression subset (Pratt parser: `let`, `if`/`else`, `fn` closures,
application, arithmetic, comparisons, `and`/`or`/`not`, boolean/int literals).
It returns the evaluated value for the default test expressions, and
type-checks + evaluates stdin input when piped.

`compile_hex.nula` is the bytecode emitter: a Nulang Core program that
compiles the same Core subset to hex-encoded `.nbc` instructions with marker
comments for jump targets, constant-pool entries, and closure frames.

The pipeline (no Rust compiler in the loop after the host builds
`compile_hex.nula` once):

```
echo '1 + 2 * 3' |
  nulang bootstrap/compile_hex.nula |   # Nulang Core → hex + markers
  python3 bootstrap/fixup_hex.py  |     # patch jumps / constants / closures
  python3 bootstrap/hex2nbc.py    > out.nbc
nulang out.nbc                          # VM runs the compiled program → 7
```

The Rust host (`nulang`) runs `compile_hex.nula`; the hex output is converted
to `.nbc` binary (RFC 0001) by `fixup_hex.py` + `hex2nbc.py`.

### Stage 2 (Restricted Core self-compile oracle passes; full self-hosting pending)
The Core compiler source in compile_hex.nula can be translated by prep_core.py,
compiled into .nbc, then used to compile test expressions. Bootstrap check 7
compares the generated hex against the Rust-hosted execution of the same Nulang
compiler on six reference programs, including recursion and closures.

The latest verified check runs all seven bootstrap phases successfully. This
is a restricted Core **self-compilation proof**, not full-language self-hosting:
the compiler still needs Python preprocessing and fixup tools, and the VM is
built with Rust. See verify.sh and RFC 0003 for the exact proof boundary.

### Stage 3 (Core VM implemented in Rust; independent host planned)
The --backend core-vm interpreter exists as an isolated frozen-Core execution
path. It is still implemented in Rust; bootstrapping an independent VM on
other architectures and removing Python and Rust build requirements remains
future work. Do not equate the existence of core-vm with full independence.

## Directory Structure

```
bootstrap/
├── README.md            # This file
├── self_test.nula       # Minimal Nulang Core test program (fib(10) = 55)
├── compiler_core.nula   # Stage 12: Core parser + type-checker (evaluates)
├── compile_hex.nula     # Stage 12: Core → hex bytecode emitter
├── compile_arith.nula   # Earlier arithmetic-only emitter
├── emitter.nula         # Stage 1: JSON emitter for .nbc generation
├── host.nula            # Placeholder host shim (returns 0 until Stage 3)
├── prep_core.py            # Preprocess compiler_core.nula for compile_hex.nula
├── desugar_fns.py          # Multi-fn programs → let-binding chains
├── fixup_hex.py            # Patch jump/constant/closure offsets in hex output
├── hex2nbc.py              # Hex text → .nbc binary
├── diagnose_self_host.py   # Diagnostic dump of the Stage 2 self-compile pipeline
├── spill_bug_repro.nula    # Register-spill regression repro
└── verify.sh               # End-to-end bootstrap verification (run: bash verify.sh)
```

## Build & Test

```bash
# Run all seven bootstrap phases, including six self-compile oracle cases.
# stdin is closed explicitly for the non-interactive verification.
bash bootstrap/verify.sh </dev/null

# Single expression through the self-hosting pipeline:
echo '1 + 2 * 3' | nulang bootstrap/compile_hex.nula |
  python3 bootstrap/fixup_hex.py |
  python3 bootstrap/hex2nbc.py > /tmp/out.nbc
nulang /tmp/out.nbc     # → 7

# Diagnose the Stage 2 self-compile oracle:
python3 bootstrap/diagnose_self_host.py [smoke_expr]
```

`verify.sh` and `diagnose_self_host.py` accept
`NULANG_BIN=/path/to/nulang bash bootstrap/verify.sh` to skip the `cargo run`
rebuild.

## Core Constraints

The emitter uses ONLY Nulang Core (RFC 0002):
- Expressions: `let`, `if`/`else`, `match`, function application, arithmetic
- Types: `Int`, `Bool`, `String`, `Unit`, `Nil`, `Vec<T>`, `Map<K,V>`, records
- Declarations: `fn`, `const`
- Effects: `IO.print` and `IO.read` only
- Capabilities: `val` only (implicit)

### Known encoding notes
- Boolean literals compile to `Const0`/`Const1` (int 0/1) rather than
  constant-pool `Constant::Bool` entries. Control flow (truthiness checks
  compare against int 0) depends on this, so the emitted value of a bare
  bool literal displays as `0`/`1` rather than `false`/`true`. Comparison
  opcodes (`==`, `<`, …) still produce tagged bools (`true`/`false`).
- Identifier hashing uses the low 16 bits of the FNV-style hash
  (`read_ident`); keyword constants in the source must be the low-16 values
  (e.g. `false` = 13715, not 79251).

## References

- RFC 0001: Format Stability (`.nbc` format)
- RFC 0002: Frozen Core
- RFC 0003: Remaining Longevity Roadmap (Item 3)
- `src/format/nbc.rs`: `.nbc` encoder/decoder
- `src/bytecode.rs`: Instruction encoding
