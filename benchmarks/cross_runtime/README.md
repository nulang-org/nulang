# Cross-runtime Savina baselines

This directory provides matched message-passing baselines for four workloads
already implemented by Nulang's `src/benchmarks.rs` harness:

| Workload | Parameters | Reported logical messages |
|---|---:|---:|
| counting | 1 actor, N=200,000 | 200,000 |
| ping_pong | N=20,000 round trips | 40,001 |
| thread_ring | 10 actors, H=20,000 token hops | 20,000 |
| fork_join | 8 workers, 50,000 tasks | 100,000 |

The companion implementations use only each platform's standard message
primitive:

- **Nulang** — real `Runtime` + bytecode actors from `src/benchmarks.rs`
- **Rust** — `std::sync::mpsc` channels + native threads
- **Go** — channels + goroutines
- **Erlang/BEAM** — native processes + mailboxes

Run all installed runtimes on the same host:

```bash
python3 scripts/cross_runtime_bench.py --runs 5 --warmup 1 \
  --cpu-mode single \
  --output /tmp/nulang-cross-runtime.json
```

The runner compiles each external fixture once, performs warm-up runs, then
records exact nanosecond timings from each runtime and reports the median for
every workload. The JSON also records OS/CPU counts, git SHA, toolchain
versions, and the CPU topology used for measurement.

## CPU-topology rule

`--cpu-mode single` is the default and is the mode intended for cross-runtime
comparison. On Linux it constrains every measured runtime process, including
all of its child threads/schedulers, to the same one logical CPU chosen from
the benchmark process's allowed CPU set. Nulang's current Savina harness uses
one runtime shard, so this prevents `fork_join` from silently comparing
single-shard Nulang with multi-core Rust, Go, or BEAM execution.

`--cpu-mode host` leaves CPU affinity unconstrained. It is useful for
diagnostics, but **host-mode fork-join results are not a fair multicore Nulang
comparison** because this harness still measures Nulang with one shard. A
separate sharded Nulang fixture is required before host-wide scaling numbers
should be compared or published.

On platforms without `sched_getaffinity`/`sched_setaffinity`, use
`--cpu-mode host`; single-core comparative runs should be produced on Linux or
another environment where the selected affinity is recorded and enforced.

## Interpretation constraints

These numbers are **baselines, not a universal language or framework
ranking**. The implementations intentionally use standard runtime primitives,
not third-party actor frameworks, and their schedulers differ materially.

In particular:

- In `single` mode every measured runtime is constrained to the same one
  logical CPU, but their scheduler designs still differ.
- Nulang's existing benchmark sends the counting/fork-join input burst and
  then drains the runtime scheduler; Rust/Go/Erlang actors may consume while
  the producer is still sending.
- Rust's baseline uses native threads, Go uses goroutines, and Erlang uses
  BEAM processes. CPU affinity makes the available compute budget comparable;
  it does not make their scheduling semantics identical.
- `thread_ring` reports the same logical hop count as Nulang's existing
  harness rather than attempting to count setup/completion control messages.
- Compilation, process startup, actor wiring, and fixture construction are
  outside the timed region where the workload permits it.
- Shared CI runners are noisy. Compare medians from the **same run and host**,
  and repeat important results on controlled hardware before publishing them.
- Do not turn one workload into a blanket "X is faster than Y" claim.

For actor-framework comparisons (for example Pony, Ractor/Actix/Kameo,
Proto.Actor, or Pekko/Akka), add separate pinned fixtures instead of silently
changing these standard-runtime baselines.
