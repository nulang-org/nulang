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
  --output /tmp/nulang-cross-runtime.json
```

The runner compiles each external fixture once, performs warm-up runs, then
records exact nanosecond timings from each runtime and reports the median for
every workload. The JSON also records OS/CPU counts, git SHA, and toolchain
versions.

## Interpretation constraints

These numbers are **baselines, not a universal language or framework
ranking**. The implementations intentionally use standard runtime primitives,
not third-party actor frameworks, and their schedulers differ materially.

In particular:

- Nulang's existing benchmark sends the counting/fork-join input burst and
  then drains the runtime scheduler; Rust/Go/Erlang actors may consume while
  the producer is still sending.
- Rust's baseline uses native threads, Go uses goroutines, and Erlang uses
  BEAM processes. Their worker/scheduler topology is not identical to Nulang.
- `thread_ring` reports the same logical hop count as Nulang's existing
  harness rather than attempting to count setup/completion control messages.
- Compilation, process startup, actor wiring, and fixture construction are
  outside the timed region where the workload permits it.
- Shared CI runners are noisy. Compare medians from the **same run and host**,
  and repeat important results on controlled hardware before publishing them.
- Do not turn one workload into a blanket "X is faster than Y" claim.

For actor-framework comparisons (for example ractor/Actix or Proto.Actor), add
separate pinned fixtures instead of silently changing these standard-runtime
baselines.
