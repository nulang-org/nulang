# Numeric arrays: Fortran-inspired implementation admission gates

**Status:** Design note / experimental proposal — not an implemented API  
**Scope:** Nulang native numerical computation; no new Frozen/Stable syntax  
**Related:** RFC 0024 (orthogonal local execution), DR-002 (Cranelift-only), DR-003 (pure placement), DR-004 (ORCA + arenas)

## Decision: establish measurable, semantics-preserving primitives first

Modern Fortran's strengths come from explicit shape/stride descriptors, predictable element types, and independent-loop contracts. Nulang should borrow the *compiler-relevant guarantees*, not Fortran syntax, coarrays, or a second mutable distributed-memory model.

The existing `[T]` / `perform Array.*` API is value-semantic and handler-visible. **Do not transparently redefine `perform Array.length` as pure**: user handlers can intercept the operation, and the effect checker must preserve that distinction.

Start with an **experimental numeric package** (library or optional host-backed component) before proposing any frozen core operation. New syntax, a new language type, or wire-format changes require explicit admission under `CONTRIBUTING.md` and `GOVERNANCE.md`.

## Phase 0: correctness prerequisites

1. Check the effect-subset proof separately from row unification. An unresolved effect tail cannot satisfy a closed `! {}` contract.
2. Preserve `perform Array.*` dispatch and handler semantics.
3. Require all source-level pure numeric operations to have an actual empty inferred effect row, and no external authority capabilities.
4. Do not infer parallel/distributed placement merely from absence of an explicitly listed effect: open effect tails remain unknown.

See the draft reviews #1957 and #1960. These are **not** evidence that full effect-row variable unification has shipped.

## Phase 1: contiguous typed buffers (experimental)

Proposed internal descriptor for a *read-only view*, not yet a public Nulang value layout:

```text
{ owner: owned_numeric_buffer, offset_elements: usize,
  length_elements: usize, stride_elements: usize, elem_kind: i64 | f64 }
```

- Start with homogeneous, contiguous `i64` and `f64`. `Float32` is planned in the language spec, not an available user-facing assumption.
- Require checked arithmetic for `offset + (length - 1) * stride`, including zero-length views and overflow.
- Views hold their owner alive; **no mutable aliasing** while a view is live. A borrowed view cannot escape its owner's lifetime.
- First iteration: read-only contiguous views (stride 1), then non-negative strided views only if a benchmark justifies them. Negative strides and broadcasting are deferred.
- Never treat a view as sendable across actors, durable snapshots, FFI, or WASM merely because the underlying bytes are contiguous. Define and test an explicit copy/ownership/serialization boundary first.
- Keep the existing `[T]` and `Array.slice` value semantics unchanged.

## Phase 2: independently optimized kernels

Provide library-level map/reduce/dot-product kernels over typed buffers. A pure scalar function may be specialized for element-wise execution only if its effects and capability requirements are closed and empty.

Optimization admission rules:

- Equal/compatible shapes are verified; shape mismatch is a deterministic error, never silent truncation.
- Alias and dependency analysis proves there is no unsafe loop-carried dependence.
- A fused loop allocates no intermediate numeric buffer when a non-escaping result can be emitted directly.
- Scalar, bytecode reference, native JIT, AOT and WASM versions agree **where supported**; an unsupported backend rejects rather than silently changes semantics.
- Reductions default to a documented evaluation order. Reassociation / parallel reduction requires an explicit floating-point policy, not implicit compiler freedom.
- Retain Cranelift. Do not add LLVM as a side effect of this work.

## Baseline to collect before optimization

`benches/vm_bench.rs` now includes `vm/array_value_set/8` and `vm/array_value_set/128`, each performing 200 `Array.set` updates to value-semantic arrays through the complete frontend and a freshly constructed VM.

Run:

```bash
cargo bench --bench bench_main -- vm/array_value_set
```

Those are **cost baselines**, not measurements of a proposed zero-copy API. They measure end-to-end VM execution and dispatch, not allocations independently; supplement them with allocation profiles when the typed-buffer prototype exists.

Follow the repository's documented same-runner A/B methodology in `benchmarks/README.md`; don't infer performance gains from shared CI runners or proposal-era claims.

## Tests required before claiming success

1. Empty buffer; first/last legal index; one-past-end; offset/stride integer overflow.
2. Retained view after original binding moves; borrowed view cannot outlive owner; mutable alias rejection.
3. Same-buffer overlapping input/output never writes incorrect results.
4. All supported backends match the bytecode VM on errors, bounds and numerical edge cases.
5. Actor/message transfer and persistence either explicitly copy/serialize or reject with a deterministic diagnostic.
6. `perform Array.length` remains handler-visible, and no new API silently deletes an effect.
7. For `f64`: signed zero, NaN, infinity, roundoff, and reduction order are specified and regression-tested.
8. Two representative kernels (map-chain and reduction) show a controlled, same-machine before/after comparison including memory/allocation measurements. Regression is a reason to defer widening the API.

## Explicit non-goals

No new `DO CONCURRENT` syntax, Fortran coarray semantics, generalized tensor broadcasting, implicit shared mutable buffers, new Cranelift competitor backend, or automatic distributed placement in this phase.
