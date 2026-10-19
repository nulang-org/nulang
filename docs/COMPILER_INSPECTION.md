# Compiler introspection (experimental CLI diagnostics)

Nulang can print inferred declaration types, interprocedural function effect rows,
and pre-codegen MIR structural counts without emitting a native artifact,
executing the program, or changing runtime semantics.

```sh
nulang --check examples/01_hello.nula --inspect-types
nulang --check examples/01_hello.nula --inspect-effects
nulang --check examples/01_hello.nula --inspect-types --inspect-effects
nulang --check examples/01_hello.nula --inspect-mir
nulang --check examples/01_hello.nula --inspect-types --inspect-effects --inspect-mir
```

All inspection flags require `--check <FILE>`. If frontend checking (or
MIR lowering, when requested) fails, the command exits nonzero and does not
print any partial report.
The ordinary `nulang --check <FILE>` output is unchanged.

The type report lists declarations recorded in the compiler's existing
`TypeChecker::inferred_decl_types` map, sorted by declaration name.
Imported declarations may appear. Type-variable labels come from the current
compiler inference run and are not a stable serialized interface. The report
does not claim to show every local expression type.

The effect report uses the **same checked `EffectChecker` instance** that validates
normal frontend compilation. It prints function effect rows from
`EffectChecker::function_rows()` after interprocedural fixpoint propagation.
Names and displayed effect sets are sorted for readable diffs. These are
**static upper bounds** on possible effects, not observations of actually
executed effects or proof of authority granted to a runtime. The report is
limited to functions represented in the checker's module-level function map;
it does not enumerate every actor behavior or nested expression.

The MIR report lowers the checked AST through Nulang's existing HIR and MIR
pipeline and lists compiler-owned functions and actor behaviors by name.
For each it shows parameter count, MIR local-slot count, basic-block count,
statement count, handler-table count and sorted distinct nonzero source lines
recorded in the MIR statement source map (`source_lines=-` when unmapped).
The line list is partial statement-location metadata, not an exhaustive
source map of every expression or a guarantee that generated code maps to a
user declaration. These are **compiler IR shapes**,
not bytecode instructions, verified optimizations, boxing, allocations, heap
bytes or runtime measurements. Closure lifting and compiler-generated MIR
functions may be included. MIR lowering can reject a program accepted by
frontend-only `--check`; if so the CLI fails without partial output.

MIR facts do **not** establish specialization, memory ownership, escape
analysis, or performance. Do not use the reports as evidence that optimized
backends preserve bytecode VM semantics.

Inspection flags cannot be combined with `--json`: the existing
machine-readable diagnostics schema is not changed by this feature.

## Verification

```sh
cargo test --locked --test cli_inspect_types
cargo test --locked --test cli_inspect_effects
cargo test --locked --test cli_inspect_mir
cargo test --locked
cargo check --all-targets --no-default-features
cargo fmt --all -- --check
```

The next compiler-introspection increment should add source locations and
validated type/escape facts to the MIR pipeline. Allocation and JIT reports
should follow only after those analyses have well-defined semantics and
conformance coverage.
