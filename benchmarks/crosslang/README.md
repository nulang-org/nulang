# Cross-language performance suite

This directory measures Nulang against mainstream language runtimes with the
same source-level workloads.

It complements the Criterion suite in `benches/`:

- `cargo bench --bench bench_main` is the regression gate for Nulang internals.
- this suite is an **informational external comparison**, not a merge gate.
- build/front-end time is excluded for compiled implementations; Nulang
  workloads are emitted to `.nbc` once before timing, so timed samples measure
  artifact execution rather than repeated parsing/type-checking.
- each timed sample launches a fresh process. These are therefore **cold-process**
  measurements: executable/runtime startup is included, while Nulang frontend
  compilation is excluded by running a pre-emitted `.nbc` artifact.
- `actor_runtime_baseline` makes cold-start/RSS overhead visible; do not call
  these steady-state actor numbers until a persistent-process harness exists.
- every implementation must emit the same checksum before its timing is
  accepted.
- results record host and toolchain versions. Do not compare runs from
  different machines as if they were controlled measurements.

## Workloads

### `arithmetic_loop`

9,000,000 iterations of the same integer-heavy loop already used by the JIT
microbenchmarks. It exercises dispatch/tiering, integer arithmetic and division.

### `function_call_loop`

9,000,000 calls to a tiny `add(sum, i)` function. This intentionally lets each
runtime optimize the source naturally. A compiler that can inline the call
should get that benefit; Nulang therefore exposes its current call-heavy JIT
coverage gap instead of hiding it.

The iteration count keeps the checksum inside Nulang's immediate integer range
while making process startup a minority of the runtime on typical machines.



## Concurrency workloads

Actor comparisons intentionally use only runtimes with a reasonably direct
lightweight message-passing model in this first slice: Nulang actors, Go
goroutines/channels, and Erlang processes/mailboxes.

For the cross-language actor rows, scheduler parallelism is normalized to one
logical execution lane: Nulang uses `NULANG_SHARDS=1`, Go uses
`GOMAXPROCS=1`, and Erlang uses `+S 1:1`. This isolates per-core
runtime/mailbox efficiency. Multicore scaling belongs in a separate benchmark
dimension rather than being mixed into the one-core comparison.

### `actor_mailbox`

One state-owning actor/task receives 250,000 ordered increment messages and then
a report message. The reported count must be exactly 250,000. This measures
end-to-end enqueue + mailbox drain + state-update cost. Scheduler behavior is
allowed to be native to each runtime: Nulang's single-shard CLI batches actor
turns after the top-level program yields, while Go and BEAM may run the receiver
concurrently with the producer.

### `actor_self_chain`

One actor/task executes 250,000 turns. Each turn sends exactly one message to
its own mailbox to schedule the next turn. The final checksum is the number of
completed turns. This removes producer/consumer overlap and is primarily a
mailbox + scheduler-turn benchmark.

These are semantic comparisons, not claims that the implementations have
identical scheduler architecture. True Nulang cross-shard/cross-thread cost is
measured separately inside the native runtime benchmark suite.

### `actor_fanout_fanin`

64 workers each receive 2,000 ordered work messages and then send their final
counter to one collector. The collector must report 128,000. This exercises
scheduler fairness, many live mailboxes, actor-to-actor result messages, and
fan-in contention rather than a single hot mailbox.

### `actor_runtime_baseline`

Starts each actor-capable runtime with the same one-core scheduler policy,
creates no actors/tasks/processes, prints `0`, and exits. It is not a throughput
benchmark; it provides cold-process timing and RSS baselines for interpreting
the actor workloads.

### `actor_spawn_100k`

Creates 100,000 lightweight execution entities that remain resident until
process exit: Nulang actors, Go goroutines, or Erlang processes. This measures
creation/registration cost. The suite also records peak process RSS. For this workload the report subtracts
the matching `actor_runtime_baseline` RSS and reports incremental bytes per
entity, making creation throughput and memory density visible together. CI uses 100,000 entities rather than one million to
avoid turning shared-runner memory capacity into the benchmark; million-actor
scaling belongs on a controlled high-memory host.

## Languages

The baseline suite has implementations for:

- Nulang (default bytecode + tiered JIT backend)
- Rust
- C++
- Go
- Java
- Node.js
- Python
- Erlang/OTP (actor workloads only)

Missing comparison-language toolchains are reported and skipped locally. Nulang
itself is required: the harness resolves the release CLI through Cargo metadata
(to respect a configured `target-dir`) and fails if it cannot be found. CI
builds Nulang before running the suite.

## Run locally

Build the optimized Nulang CLI first:

```bash
cargo build --release --bin nulang
python3 benchmarks/crosslang/run.py --warmups 2 --runs 7
```

Filter languages or workloads when iterating:

```bash
python3 benchmarks/crosslang/run.py \
  --languages nulang,rust,go \
  --workloads arithmetic_loop \
  --runs 5
```

Write machine-readable and Markdown reports:

```bash
python3 benchmarks/crosslang/run.py \
  --json-out /tmp/nulang-crosslang.json \
  --markdown-out /tmp/nulang-crosslang.md
```

The Markdown `time ratio vs Nulang` column is:

```text
implementation median / Nulang median
```

So `0.80x` means the implementation used 80% of Nulang's wall-clock time,
while `1.20x` means it used 120%.

## Memory measurement

On systems with GNU `/usr/bin/time`, the harness performs one extra
checksum-validated execution per implementation and records maximum resident
set size (peak RSS). This probe is separate from timed samples, so the wrapper
does not affect reported wall-clock medians. Peak RSS includes the language
runtime, JIT/VM, benchmark state, and spawned actors/tasks; compare it only
between runs on the same OS/toolchain environment.

## Interpretation rules

Do not turn one kernel into a language-wide performance claim. In particular:

1. Compare runs only on the same host, governor and toolchain set.
2. Treat shared GitHub runners as trend data, not publication-quality evidence.
3. Report median + median absolute deviation (MAD) and the individual samples.
   With seven process samples, a claimed p95 is effectively just the maximum and
   is intentionally not reported.
4. Keep checksums and source-level work equivalent.
5. Do not disable safety/runtime semantics in one language solely to win a
   benchmark.
6. Add actor, allocation, HTTP, RESP, durability and distributed workloads as
   separate suites with explicit semantic contracts rather than forcing them
   into a misleading scalar benchmark.

For publication-quality numbers, run this suite on a pinned bare-metal machine,
pin CPU frequency/affinity, record kernel/microcode/toolchain versions, and take
enough samples to report confidence intervals.
