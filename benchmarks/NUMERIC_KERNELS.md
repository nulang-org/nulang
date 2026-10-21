# Numerical kernel baselines (MATLAB-inspired first slice)

This suite is a measurement and correctness baseline, **not** a tensor API,
matrix library, GPU backend, or claim of SIMD speedup.

## Workloads

- `sum`: build an integer array of length N filled with 7 and sum its elements.
  Expected result: `7 * N`.
- `dot`: build two integer arrays filled with 2 and 3 and accumulate their
  dot product. Expected result: `6 * N`.

Lengths: 128 and 4096 in Criterion. `tests/numeric_kernels.rs` also covers
zero and singleton lengths, negative elements, and interpreter/JIT result parity.

These benchmarks are opt-in through the `numeric-bench` feature so standard benchmark CI does not incur additional samples. They use the **real Nulang frontend and VM**. Compilation occurs
before timing. However, `Array.new` executes inside the timed program, so
measurements include array allocation, VM dispatch, indexing, arithmetic,
reductions, and (in cold-JIT mode) potential compilation. They do **not**
isolate native SIMD instruction cost, and `jit_enabled_repeated` does not
assert that a region was compiled or vectorized.

## Run

Correctness first:

```sh
cargo test --locked --no-default-features --test numeric_kernels
cargo test --locked --no-default-features --features native-codegen --test numeric_kernels
```

Then run controlled Criterion baselines on the **same machine**:

```sh
cargo bench --locked --no-default-features --features numeric-bench --bench bench_main -- numeric_kernel
cargo bench --locked --no-default-features --features native-codegen,numeric-bench --bench bench_main -- numeric_kernel
```

Measurements are reported as elapsed time and elements/second. Keep interpreter,
JIT first-run, and JIT repeated samples separate. The JIT enabled/disabled
comparison must use the same source and feature profile. Do not compare results
from different shared CI machines as evidence of an optimization.

## Promotion gate before adding a numerical library

1. Both numeric-kernel integration tests pass, including lengths 0 and 4096,
   on interpreter and JIT-enabled configurations.
2. Benchmark targets build on minimal and native-JIT configurations.
3. Collect on-host measurements and verify whether native tiering actually
   compiles regions. No SIMD claim without recorded compiler evidence.
4. Profile allocation count, bytes copied, and JIT time before choosing
   typed-buffer representation, operation fusion, or external BLAS integration.
5. Avoid new language syntax, tensor ABI, dependencies, or core runtime changes
   until the measurements demonstrate a high-value bottleneck.

Future phases (not implemented here): an optional numeric-buffer package;
shape/stride semantics; vector and matrix operations; backend differential
correctness; actor-to-actor immutable buffer sharing; and optional BLAS/GPU
acceleration.
