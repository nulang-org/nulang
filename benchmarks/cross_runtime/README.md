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

- **Nulang bytecode** (`nulang`) — real `Runtime` + bytecode actors, compiled without native-codegen
- **Nulang warmed JIT** (`nulang-jit`) — currently **not a validated Savina backend**: the four short actor handlers do not prove native compilation. A separate compute-heavy Nulang actor probe tests actual JIT tier-up; the Savina comparison refuses `nulang-jit` to prevent mislabeled results.
- **Nulang AOT** (`nulang-aot`) — the same Nulang source and MIR, Cranelift ahead-of-time native actor behaviors registered before VM actor spawning; the harness rejects missing native dispatch targets rather than silently reporting bytecode execution as AOT
- **Rust** — `std::sync::mpsc` channels + native threads
- **Go** — channels + goroutines
- **Erlang/BEAM** — native processes + mailboxes

Run the matched message-passing baselines on the same host:

```bash
python3 scripts/cross_runtime_bench.py --runs 10 --warmup 1 \
  --runtimes nulang,nulang-aot,rust,go,erlang \
  --cpu-mode single \
  --output /tmp/nulang-cross-runtime.json
```

The runner compiles each external fixture once, performs warm-up runs, then
records exact nanosecond timings from each runtime and reports the median for
every workload. Measured rounds counterbalance runtime order by rotating each
pair and reversing its second round. Over **2 × the number of selected runtimes**
measured rounds (ten for the five listed runtimes), every runtime occupies each
execution position twice. Smaller run counts are only partially balanced.
Warm-ups do not shift the measured-round schedule. All Nulang binaries are
built outside timing and copied to distinct paths, so changing Cargo feature
sets cannot overwrite or silently relabel a measured executable. Backend
compilation time is excluded from the per-message throughput values.
The separately labeled compute-actor JIT mode requires nonzero actor-VM
compiled-region telemetry after untimed workload-specific message warmup;
enabling `native-codegen` alone is insufficient. That proof establishes that
the JIT tier engaged somewhere in the actor VM, **not** that all timed
instructions ran as native code. Successful CI execution and equivalent final
actor state are still required before presenting comparative data. The JSON records
`measured_execution_orders` so comparisons can audit actual execution order.
The JSON also records OS/CPU counts, git SHA, toolchain versions, and the CPU
topology used for measurement.

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

## Compute-heavy actor backend comparison (Nulang only)

The standard four Savina-style message handlers may be too short to trigger
native JIT compilation. The matched Rust, Go and Erlang comparison therefore
uses Nulang bytecode and AOT by default. It never labels uncompiled actor
handlers as a successful warmed-JIT measurement.

A second, explicitly **Nulang-only** `actor_compute` workload uses the
identical source for bytecode, warmed JIT and native AOT. Each of 128 actor
messages executes a 4,096-iteration integer loop and updates actor state.
All three backends execute one identical untimed actor warmup message and
reset state before measurement. The JIT variant additionally requires a **positive increase** in
compiled-region telemetry from the scheduler-owned actor VM during that
workload's untimed warmup; the JSON report records both the total and the
newly compiled region counts. A nonzero historical total alone is rejected.
The AOT mode requires full actor behavior target coverage. Every backend checks the exact
resulting integer state after the measured messages.
Time includes enqueue and scheduler drain but excludes parsing, compilation
and JIT warmup.

```bash
python3 scripts/cross_runtime_bench.py --workloads actor_compute \
  --runtimes nulang,nulang-jit,nulang-aot \
  --runs 12 --warmup 1 --cpu-mode single \
  --output actor-backend-results.json
```

This is a **compute-bearing actor backend diagnostic**, not a fair
cross-language or pure message-throughput result. The JIT proof establishes
that at least one actor-VM region compiled, not the percentage of timed
instructions actually executed as native code. Do not publish results before
the exact-head Rust tests and CI benchmark execution complete.
