# Compiler type inspection (experimental CLI diagnostics)

Nulang can print inferred declaration types and interprocedural function effect rows without compiling a
native artifact, executing the program, or changing runtime semantics.

```sh
nulang --check examples/01_hello.nula --inspect-types
nulang --check examples/01_hello.nula --inspect-effects
nulang --check examples/01_hello.nula --inspect-types --inspect-effects
```

Both inspection flags require `--check <FILE>`. If frontend checking
fails, the command exits nonzero and does not print a partial type report.
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

The CLI currently exposes **type and effect inspection only**. It does **not** measure heap
allocations, JIT specialization, boxing, inferred interprocedural effects, or
performance. Do not use the report as evidence that optimized backends
preserve bytecode VM semantics.

Neither `--inspect-types` nor `--inspect-effects` can be combined with `--json`: the existing
machine-readable diagnostics schema is not changed by this feature.

## Verification

```sh
cargo test --locked --test cli_inspect_types
cargo test --locked --test cli_inspect_effects
cargo test --locked
cargo check --all-targets --no-default-features
cargo fmt --all -- --check
```

The next compiler-introspection increment should consume compiler-owned typed
HIR/MIR facts and add explicit evidence for effect rows and source locations.
Allocation and JIT reports should follow only after those analyses have
well-defined semantics and conformance coverage.
