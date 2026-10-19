# Experimental Nulang Numeric Buffer

A small, dependency-free Rust prototype for Fortran-inspired **contiguous** numeric
buffers and **read-only zero-copy views**.

This is **not a Nulang language feature** and is not currently integrated with
the bytecode VM, JIT, AOT, WASM, actors, or durability. It neither changes the
existing `[T]` value semantics nor makes `perform Array.*` operations pure.

## Current scope

- Owned homogeneous `i64` and `f64` storage backed by `Arc<[T]>`.
- Safe half-open slice views that retain ownership and remain valid after the
  source buffer variable is dropped.
- Read-only nested views, checked bounds and defensive offset arithmetic.
- No mutable borrowing, strided access, FFI exports, actor transfer, or
  serialization.

## Validation

From the repository root:

```bash
cargo fmt --manifest-path crates/nulang-numeric/Cargo.toml --check
cargo test --manifest-path crates/nulang-numeric/Cargo.toml --all-targets
cargo clippy --manifest-path crates/nulang-numeric/Cargo.toml --all-targets -- -D warnings
```

The crate declares its own empty workspace and has no dependencies, so it
does not alter the root `Cargo.lock` or core compiler build. A dedicated CI
workflow checks it when these files change.

## Paired native-Rust performance experiment

Build and execute the opt-in benchmark on a single, otherwise idle host:

```bash
cargo run --release --manifest-path crates/nulang-numeric/Cargo.toml \
  --example compare_views > numeric-views.csv
```

On Linux, optionally pin it to one logical core with `taskset -c 2`
(adjust the CPU index for the host). The output is CSV with one row per
mode, size, and round: `size,range_len,iterations,round,mode,elapsed_ns,checksum`.

Each workload sums the same middle-half subrange of a numeric buffer. The
`view` mode creates an owned read-only `Arc` view, whereas `copy` creates
a fresh `Vec` for that subrange; both assert the same checksum. Three
buffer sizes run in eight paired rounds with alternating order and warmup.
Treat ratios as *within-host diagnostic measurements*, not universal
speedups; collect allocation and CPU evidence separately.

**Important:** This is not a Nulang source-level VM versus typed-buffer
benchmark. It compares two Rust primitives and does not capture Nulang
effect dispatch, compiler lowering, JIT, AOT, or WASM. Do not claim a Nulang
performance gain until a Nulang-level implementation and equivalent
same-runner A/B harness exist.

## Admission into Nulang

Do not add Nulang syntax, VM values, or numeric sendability as part of this
prototype. Promote only after the purity and effect-row reviews (#1957 and
#1960), backend differential equivalence, ownership contracts, and controlled
same-runner performance evidence specified in #1962.
