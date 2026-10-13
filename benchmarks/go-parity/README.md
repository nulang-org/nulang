# Nulang vs Go parity benchmarks

This directory contains equivalent small kernels for measuring the performance
gap between Nulang and Go. The goal is to guide compiler/runtime work with an
external baseline, not to produce a language-ranking headline.

## What is measured

The current corpus targets four compiler/runtime paths:

- `int_loop` — tagged/unboxed integer arithmetic and loop code generation.
- `float_loop` — floating-point code generation.
- `direct_call_loop` — 100k direct calls; this exposes Nulang's current
  helper-backed JIT call overhead and later measures native JIT-to-JIT calls.
- `fib25` — recursive calls; this intentionally exposes the current JIT
  recursion gate and will show the payoff from a safe native recursion ABI.

Nulang is measured in two modes:

- **warm JIT**: source is compiled once, one untimed execution tiers the VM up,
  then Criterion measures another execution on the same JIT session.
- **AOT**: MIR is compiled once with Cranelift and Criterion measures
  `AotModule::run()`. That call still includes Nulang's fixed standalone-heap
  setup, so very small kernels should not be over-interpreted.

The Go direct-call kernel marks the callee `//go:noinline` so it actually
measures call overhead rather than letting the optimizer erase the comparison.

## Run

Requirements: the pinned Rust toolchain, Cargo, Python 3, and Go.

```bash
python3 benchmarks/go-parity/compare.py
```

To rerun only Go and reuse existing Criterion data:

```bash
python3 benchmarks/go-parity/compare.py --no-run
```

Or run each side manually:

```bash
cargo bench --bench bench_main -- go_parity
cd benchmarks/go-parity
go test -run='^$' -bench=. -benchmem -count=5
```

The comparison script prints Nulang/Go ratios. A ratio below 1.0 means Nulang
was faster on that kernel on that machine.

## Measurement rules

Use the same machine and keep these stable between runs:

1. release/toolchain revision;
2. CPU governor / performance mode;
3. CPU affinity when pinning is used;
4. background load;
5. benchmark input sizes.

Do not compare CLI wall-clock startup against Go's in-process benchmark
functions. Do not use these microbenchmarks as claims about application-level
performance; add representative HTTP, actor, allocation, and distributed
workloads separately.
