# Read-only aggregate allocation report

`nulang explain allocations` helps establish a measurable baseline before
changing Nulang's compiler, ARC/ORCA ownership logic, or arena placement.

## Usage

Build a bytecode artifact and inspect it without running the program:

```sh
nulang --emit-nbc program.nula
nulang explain allocations program.nbc > allocations.json
```

The command accepts a **validated `.nbc` artifact**, not source or native
object code. Malformed, truncated, or incompatible artifacts fail before
analysis. Output is JSON on stdout; errors go to stderr with a nonzero exit
status.

Output schema `1` includes the module name, aggregate candidate counts, and
per-site records in ascending zero-based bytecode PC order:

- `arena_eligible` — the existing conservative iso-arena proof establishes
  activation-local lifetime for that allocation site.
- `not_proven` — the proof cannot establish non-escape on all relevant paths.
  It is *not* evidence that a heap allocation occurred.
- `not_analyzed` — the opcode is intentionally outside the arena-eligibility
  analysis (currently `RecCopy`, whose child-retain semantics require
  separate handling).

The scanner covers `ArrAlloc`, `RecMk`, `TupleMk`, and `RecCopy`.
It deliberately **does not** claim to count string allocations, effect
handlers, native/WASM runtime allocations, or allocations inside library
helpers. Reporting a site as eligible does not imply that the arena was
actually used: `NULANG_ISO_ARENA` enables the optional arena path and runtime
behavior depends on the execution backend and control flow.

This command is read-only. It does not execute bytecode, start actors, alter
the collector, change artifacts, or change generated code. The report
reuses `iso_arena::qualifying_alloc_sites` to avoid divergence between the
diagnostic and the runtime proof.

## Baseline workflow

For a reproducible comparison, keep the same input artifact and compiler
revision while collecting the report. Track total candidates and dispositions
alongside an independent runtime allocation benchmark. Compare reports by
bytecode PC only when the input `.nbc` is identical; PCs can change after
compilation and optimization changes.

Suggested validation before promoting an optimizer:

```sh
cargo test --test allocation_report
cargo test allocation_report_cli
cargo test iso_arena
cargo test
cargo fmt --all -- --check
```

A future runtime instrumentation pass should report actual heap/arena
allocations, retain/release operations, and bytes by workload. **Do not use
the static eligibility count as a performance metric.**
