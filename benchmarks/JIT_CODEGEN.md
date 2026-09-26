# JIT codegen benchmark

Nulang's native JIT exposes backend-neutral compiler telemetry through
`JitBackend::compile_stats` and `VM::jit_compile_stats`. The dedicated
`nulang-jit-bench` runner combines those compiler-only counters with cold and
warm end-to-end execution timing.

The purpose is to compare native backends and tiering policies without mixing
compiler latency into runtime throughput or relying on backend-specific APIs.

## Run

Human-readable output:

```bash
cargo run --release --bin nulang-jit-bench -- --format human
```

Machine-readable output:

```bash
cargo run --release --bin nulang-jit-bench -- \
  --format jsonl --repeat 5
```

Run one workload with `--workload numeric_loop`, `call_loop`, or
`branch_loop`. Use `--list` to print the workload names.

The `JIT codegen benchmarks` workflow runs the correctness/telemetry
regression and emits the JSONL runner output as an artifact. Pull requests use
one repetition as a validation signal; manual dispatches default to five.
Shared-runner absolute compiler timings are diagnostic rather than a
longitudinal performance gate—use same-host comparisons or controlled hardware
for optimization decisions.

## Metrics

Each JSONL row contains:

- `first_run_ns`: end-to-end execution on a fresh VM, including interpreter
  warm-up and any first-tier compilation.
- `warm_run_ns`: a second execution on the same VM with installed native
  code retained.
- `compiled_regions` and `typed_regions`: installed region counts after
  both runs.
- `first_compile.fast_*` / `first_compile.optimized_*`: cumulative
  successful native compilation count and compiler wall time after the first
  run.
- `warm_compile_delta.*`: additional compiler work performed during the warm
  run.

Compiler time measures only the interval around the native backend compile
request. Region planning, interpreter warm-up, and generated-code execution
remain visible in end-to-end run time rather than being charged to the
compiler.

## Correctness

Before timing JIT execution, each workload runs once with `VM::new_without_jit`.
Both timed JIT results are compared against that interpreter result. A faster
backend that changes program semantics therefore fails the benchmark run.

## Backend comparisons

The schema intentionally contains no Cranelift-specific fields. A MIR,
copy-and-patch, or custom native backend can implement the same
`JitBackend::compile_stats` contract and use this runner unchanged. Compare
backends on the same commit, workload, build profile, hardware, and repetition
count; report both compile time and execution time rather than a single blended
number.
