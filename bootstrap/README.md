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

### Stage 2 (Restricted-subset self-compilation verified historically)
The Nulang-written `compile_hex.nula` emitter can compile its own
`prep_core.py`-transformed source into executable `.nbc`. Check 7 in
`bootstrap/verify.sh` is enabled and compares its outputs with the host-run
emitter over six Core expressions. Commit
[`0c19d60`](https://github.com/nulang-org/nulang/commit/0c19d60a01ad5679ebfc3bb64947fcb98141738b)
records this oracle passing on 2026-09-11. The earlier disabled-check-7
investigation in `CHECK7_STATUS.md` is historical, not the current gate.

This is **not a fully self-hosted Nulang compiler**: the bootstrap accepts a
restricted expression subset rather than full modules, types and effects;
Python still performs source preprocessing and bytecode fixups, and the
Rust-implemented Nulang VM executes the resulting compiler. The independent
CI gate and its evidence limitations are described in [STATUS.md](STATUS.md).

### Stage 3 (Planned — Independence)
The Stage 2 `.nbc` is run on a minimal Core VM (pure interpreter, no JIT, no
WASM, no AI, no Python — just the frozen Core opcodes). That VM can be ported
to new hardware without the Rust toolchain, achieving full host independence.

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
├── verify.sh               # Existing seven-check bootstrap verification
├── verify_differential.py  # Stage 1 vs Stage 2 emitted code + executed output
├── fixtures/differential.json # Curated expected-outcome corpus
└── STATUS.md               # Evidence, limitations, and next gates
```

## Build & Test

```bash
# Run existing checks 1–7 (includes the enabled Stage 2 self-compile oracle).
NULANG_BIN="$PWD/target/debug/nulang" bash bootstrap/verify.sh

# Run the independent bootstrap-generation differential corpus.
python3 -m unittest discover -s scripts/tests -p test_bootstrap_differential.py
python3 bootstrap/verify_differential.py --nulang "$PWD/target/debug/nulang"

# Single expression through the self-hosting pipeline:
echo '1 + 2 * 3' | nulang bootstrap/compile_hex.nula |
  python3 bootstrap/fixup_hex.py |
  python3 bootstrap/hex2nbc.py > /tmp/out.nbc
nulang /tmp/out.nbc     # → 7

# Diagnose the Stage 2 self-compile oracle:
python3 bootstrap/diagnose_self_host.py [smoke_expr]
```

`verify.sh` and `diagnose_self_host.py` accept
`NULANG_BIN=/path/to/nulang` to skip the `cargo run` rebuild. Build the CLI
first with `cargo build --locked --bin nulang`. The differential runner
requires `--nulang` and uses temporary output files rather than modifying the
checkout. The checked-in fixture set is intentionally small and does not
establish Rust-frontend equivalence.

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
