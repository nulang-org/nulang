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

- **Nulang** — real `Runtime` + bytecode actors from the standalone `nulang-savina` runner
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

`--cpu-mode single` is the default and remains the one-core comparison mode.
On Linux it constrains every measured runtime process, including all child
threads/schedulers, to the same logical CPU chosen from the benchmark process's
allowed CPU set. In this mode Nulang fork-join is forced to one shard.

`--cpu-mode host` leaves CPU affinity unconstrained. Host mode still defaults
to one Nulang shard, so it remains diagnostic unless the sharded fork-join
fixture is selected explicitly. For example:

```bash
python3 scripts/cross_runtime_bench.py --runs 5 --warmup 1 \
  --cpu-mode host --nulang-shards 4 \
  --output /tmp/nulang-cross-runtime-host.json
```

`--nulang-shards` accepts 1–8 and affects **only Nulang fork-join**. The other
Nulang workloads remain single-shard. The report records both
`nulang_fork_join_shards` and `nulang_non_fork_join_shards`, and the harness
rejects multi-shard Nulang when `--cpu-mode single` is selected.

On platforms without `sched_getaffinity`/`sched_setaffinity`, use
`--cpu-mode host`; single-core comparative runs should be produced on Linux or
another environment where the selected affinity is recorded and enforced.

## Interpretation constraints

These numbers are **baselines, not a universal language or framework
ranking**. The implementations intentionally use standard runtime primitives,
not third-party actor frameworks, and their schedulers differ materially.

In particular:

- In `single` mode every measured runtime is constrained to the same one
  logical CPU, but their scheduler designs still differ. In host mode, choose
  `--nulang-shards` intentionally for the machine being measured; the harness
  does not infer a supposedly "fair" shard count from host CPU count.
- Nulang counting and single-shard fork-join send their input burst before
  draining the runtime scheduler; Rust/Go/Erlang actors may consume while the
  producer is still sending. The opt-in sharded Nulang fork-join instead keeps
  a bounded 512-task in-flight window so real shard threads can drain the
  bounded cross-shard transport concurrently. The logical workload remains
  50,000 tasks plus 50,000 acknowledgements, but producer scheduling semantics
  are therefore not identical.
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
