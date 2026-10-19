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

Run one workload with `--workload NAME`. The numeric crossover set covers
1k, 2k, 3k, 4k, 5k, 7.5k, 10k, and 100k loop trips, plus `call_loop` and
`branch_loop`. Use `--list` to print the exact workload names.

The `JIT codegen benchmarks` workflow runs the correctness/telemetry
regression and emits the JSONL runner output as an artifact. Pull requests use
one repetition as a validation signal; manual dispatches default to five.
Shared-runner absolute compiler timings are diagnostic rather than a
longitudinal performance gate—use same-host comparisons or controlled hardware
for optimization decisions.

## Metrics

Each JSONL row (schema 2) contains:

- `interpreter_run_ns`: the same compiled module executed on a fresh
  interpreter-only VM.
- `first_run_ns`: end-to-end execution on a fresh JIT-enabled VM, including interpreter
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

Each measured repetition times the interpreter-only run as well as first-run
and warmed JIT execution. Both JIT results are compared against that
interpreter result, so a faster backend that changes program semantics fails
the benchmark run. The dense numeric workloads make the first-run
JIT/interpreter crossover directly observable instead of inferring it from
separate shared-runner histories.

## Backend comparisons

The schema intentionally contains no Cranelift-specific fields. A MIR,
copy-and-patch, or custom native backend can implement the same
`JitBackend::compile_stats` contract and use this runner unchanged. Compare
backends on the same commit, workload, build profile, hardware, and repetition
count; report both compile time and execution time rather than a single blended
number.

## Generic-call specialization candidates

The `concrete_identity_call_loop` and `generic_identity_call_loop` workloads
execute the same 50,000-iteration integer accumulation. They differ only in the
identity function's concrete versus generic signature. Each benchmark repetition
checks its interpreter, first JIT run, and warm JIT run for equal results.

Run the pair on the **same pinned CPU**, using the same commit, build profile,
repetition count and warm-up policy:

```bash
cargo run --locked --release --no-default-features --features native-codegen \
  --bin nulang-jit-bench -- --workload concrete_identity_call_loop --format jsonl --repeat 8
cargo run --locked --release --no-default-features --features native-codegen \
  --bin nulang-jit-bench -- --workload generic_identity_call_loop --format jsonl --repeat 8
```

These workloads are diagnostic baselines for potential generic specialization.
They do **not** implement monomorphization, prove an optimization, or justify a
generic-language ABI change. Execute a paired base/candidate benchmark before
changing compilation policy; keep both the cold compilation cost and the
warmed execution cost visible.


### Counterbalanced comparison gate

The one-repetition JIT telemetry workflow is a smoke/contract run, not enough
to quantify generic dispatch overhead or justify monomorphization. Run the
counterbalanced benchmark after the paired fixtures from #1964 are available:

```bash
python3 -m unittest discover -s scripts/tests -p 'test_jit_generic_call_ab.py' -v
python3 scripts/jit_generic_call_ab.py --rounds 8 --output jit-generic-call-ab.json
```

The comparator builds the release binary once and then executes that binary
directly with the measured process pinned to one available logical CPU on
Linux by default. Use `--cpu-mode host` only for an explicitly unpinned
exploratory comparison or where affinity is unsupported. The JSON report
records the CPU mode and selected CPU affinity. Each round executes both
workloads and alternates which one runs first. Each individual benchmark
process verifies interpreter/first-JIT/warm-JIT result
parity before emitting JSONL; the analyzer rejects missing or malformed metrics
and preserves every raw sample alongside median interpreter, first-run, warm-run
and compilation durations. CI uploads the report as an artifact.

The median of same-round **generic/concrete** warm-duration ratios is
descriptive only. Both programs do the same work; neither program uses actual
monomorphization. Each process constructs its own JIT and measures a short hot
loop, so OS scheduling, CPU frequency, process startups, and JIT policy can
affect small differences. The result does not establish statistical significance,
a general compiler speedup, or the benefit of adding a new optimization.
A proposed monomorphization patch must be measured separately against its
exact base SHA on the same controlled host, including compilation/code-size
cost and interpreter/backend semantic parity.
