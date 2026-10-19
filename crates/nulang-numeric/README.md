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

## Admission into Nulang

Do not add Nulang syntax, VM values, or numeric sendability as part of this
prototype. Promote only after the purity and effect-row reviews (#1957 and
#1960), backend differential equivalence, ownership contracts, and controlled
same-runner performance evidence specified in #1962.
