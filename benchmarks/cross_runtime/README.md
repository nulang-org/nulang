# Cross-runtime stateless actor baselines

This directory provides matched message-passing baselines for four workloads
already implemented by Nulang's `src/benchmarks.rs` harness. It now reports
**two distinct comparator classes**: native runtime primitives and a full actor
framework. They must not be blended into a generic language leaderboard.

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
- **Ractor 0.16.5** — native Rust actor framework (Tokio single-thread executor)

Ractor is built from an isolated Cargo workspace under `ractor_baseline/`:
its dependencies do not change Nulang's main `Cargo.toml` or lockfile.
The exact direct dependency is pinned; the fixture's generated `Cargo.lock`
captures the resolved transitive dependencies. Save that file alongside every
result if the fixture lockfile has not yet been committed.

Run all installed runtimes on the same host:

```bash
python3 scripts/cross_runtime_bench.py --runs 5 --warmup 1 \
  --cpu-mode single \
  --output /tmp/nulang-cross-runtime.json
```

The runner builds each external fixture once, performs warm-up runs, then
records elapsed nanoseconds from each runtime and reports the median for
every workload. The JSON also records OS/CPU counts, git SHA, toolchain
versions, the CPU topology, comparator category, and the expected
logical-message counts. It rejects missing, duplicated, zero-duration, or
incorrect-message-count records instead of silently reporting them.

Run the independent ingestion tests with:

```bash
python3 -m unittest discover -s tests -p test_cross_runtime_bench.py -v
```

Run the baseline runtimes without Ractor by passing
`--runtimes nulang,rust,go,erlang`, or the actor-framework pair with
`--runtimes nulang,ractor`. The default runs all five runtimes.

## CPU-topology rule

`--cpu-mode single` is the default and is the mode intended for cross-runtime
comparison. On Linux it constrains every measured runtime process, including
all of its child threads/schedulers, to the same one logical CPU chosen from
the benchmark process's allowed CPU set. Nulang's current Savina harness uses
one runtime shard, and the Ractor comparator explicitly uses a single
Tokio execution thread, so this prevents `fork_join` from silently comparing
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
ranking**. Rust/Go use standard concurrency primitives; Ractor uses a
third-party actor framework; Erlang/Nulang use actor-language runtimes.
Even within the actor category, message semantics and scheduling differ.

In particular:

- In `single` mode every measured runtime is constrained to the same one
  logical CPU, but their scheduler designs still differ.
- Nulang's existing benchmark sends the counting/fork-join input burst and
  then drains the runtime scheduler; Rust/Go/Erlang actors may consume while
  the producer is still sending.
- Rust's baseline uses native threads, Go uses goroutines, Erlang uses
  BEAM processes, and Ractor uses Tokio actor tasks. CPU affinity makes the
  available compute budget comparable; it does not make their semantics
  identical.
- Ractor's current-thread Tokio executor processes queued work after the
  synchronous producer loop yields; this resembles Nulang's burst-then-drain
  counting/fork-join path more closely than Go/Erlang, but is not identical.
- Both Ractor and Nulang are measured on a single execution worker; host mode
  is diagnostic, not a multicore actor-scaling comparison.
- `thread_ring` reports the same logical hop count as Nulang's existing
  harness rather than attempting to count setup/completion control messages.
- Compilation, process startup, actor wiring, and fixture construction are
  outside the timed region where the workload permits it.
- Shared CI runners are noisy. Compare medians from the **same run and host**,
  and repeat important results on controlled hardware before publishing them.
- Do not turn one workload into a blanket "X is faster than Y" claim.

For other actor-framework comparisons (for example Actix, Kameo,
Proto.Actor, or Pekko/Akka), add separate pinned fixtures rather than
silently changing standard-runtime baselines. Do not introduce a
cross-language performance gate on shared CI runners.

## Actor creation and resident-memory companion

Use the existing Nulang `examples/actor_density.rs` probe and Ractor's
`ractor_baseline/src/bin/density.rs` for single-host actor-density diagnostics:

```bash
python3 -m unittest discover -s tests -p test_actor_density_cross_runtime.py -v
python3 scripts/actor_density_cross_runtime.py \
  --actors 1000,10000 --runs 5 --warmup 1 --cpu-mode single \
  --output /tmp/nulang-ractor-density.json
```

The runner builds both binaries once and executes **each sample in a fresh
process**. It reports median spawn operations/sec and the difference between
Linux process RSS before actor creation and after actor startup settles. If RSS
is unavailable, does not grow, or declines, it reports `null`, not zero.

**Lifecycle caveat:** Nulang's `Runtime::spawn_actor` and Ractor's
`ActorRuntime::spawn_instant` both return before message handling, but
Ractor completes asynchronous pre-start work during the separate settle stage.
They are different APIs. The spawn and memory results are diagnostics,
not apples-to-apples proof of runtime superiority. Warmups and all three
RSS measurements are produced by independent processes.

The original `benchmarks/ACTOR_DENSITY.md` still owns Nulang's actor-density
ladder (10k to 1M), exact-base regressions, and existing BEAM comparator.
The cross-runtime density companion does not replace those tests. Run the
new sampler on controlled hardware before treating RSS deltas as representative.

### Vocabulary

These are **non-durable actor workloads**, not mathematically stateless actors:
ping-pong, counting, and fork-join workers mutate private local counters,
but do not use Nulang's durable workflow/journal subsystem. Keep statefulness
and persistence separate when interpreting results.

### Runtime execution order

Each measurement round runs every selected runtime. The runner rotates their
sequence every round, so over one complete cycle each runtime occupies each
execution position once. This reduces systematic first-run cache/thermal bias,
but **does not remove cross-run host drift**; record hardware load and compare
paired Nulang revisions on controlled hardware before making performance claims.

## Contended admission (not an actor-speed leaderboard)

Use the separate many-to-one queue benchmark for 1/2/4/8 concurrent producers:

```bash
python3 -m unittest discover -s tests -p test_mailbox_contention_cross_runtime.py -v
python3 scripts/mailbox_contention_cross_runtime.py \
  --producers 1,2,4,8 --messages-per-producer 10000 \
  --runs 5 --warmup 1 --cpu-mode host \
  --output /tmp/nulang-ractor-contention.json
```

The Nulang fixture measures **`Mailbox::push` only** with a lock-free
concurrent queue. It excludes actor-reference routing, scheduler activation,
runtime wakeups, message execution, and distributed delivery. The Ractor fixture
measures **`ActorRef::cast`**, a higher-level actor-mailbox admission path.
These are deliberately separately classified lower-bound diagnostics, not fair
end-to-end actor-vs-actor performance comparisons. Do **not** use their raw
throughput ratio as evidence of a language-speed advantage.

Both implementations construct producer threads outside timing, release them
through a barrier, send immutable one-integer messages, and require that the
exact expected count be received after the timed enqueue phase. Barrier wakeup
and producer joins remain inside timing. Neither fixture processes its actor
mailbox during the enqueue measurement. `--cpu-mode host` is preferred for
actual multicore contention. Record the host CPU topology and repeat on
controlled hardware before interpreting differences.
