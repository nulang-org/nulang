# BEAM Parity Benchmark Plan

This benchmark track answers a narrower question than the general compiler/VM
benchmarks: how does Nulang's actor runtime compare with BEAM/OTP on the
properties that make BEAM unusually strong?

The goal is not to claim parity from one number. The goal is to establish
repeatable evidence for actor density, spawn cost, mailbox throughput,
scheduler behavior, recovery, and failure handling.

## Automated baseline

Nulang's Criterion suite now includes these matched groups:

- `beam_parity/spawn_idle/{1000,10000}` — actor creation only; runtime setup is
  outside the timed region.
- `beam_parity/single_mailbox_flood/{1000,10000}` — enqueue and drain N messages
  through one already-started actor.
- `beam_parity/fanout_one_message_each/{1000,10000}` — deliver one message to N
  already-started actors and drain the scheduler.

Run them with:

```bash
cargo bench --bench bench_main -- beam_parity
```

A matching Erlang/BEAM harness lives at
`benchmarks/beam/beam_parity.escript`:

```bash
escript benchmarks/beam/beam_parity.escript
escript benchmarks/beam/beam_parity.escript 1000 10000 100000
```

Run both on the same machine, with the same CPU governor and otherwise idle
host. Do not compare GitHub-hosted Criterion results with local BEAM results;
shared-runner noise and hardware differences make that comparison invalid.

## Automated same-host BEAM evidence

The dedicated `perf/beam-parity*` benchmark PR runs a matched Nulang and
Erlang/BEAM harness on the same GitHub Actions runner for 1k and 10k actors.
Both harnesses emit the same CSV schema:

`runtime,benchmark,operations,elapsed_us,ops_per_sec`

The CI summary reports the raw throughput and Nulang/BEAM ratio for actor
spawn, single-mailbox flood, and one-message-per-actor fan-out. Results are
informational rather than a pass/fail ranking: the purpose is to establish a
dated same-host baseline and identify order-of-magnitude gaps that deserve
profiling. The raw CSV files and rendered summary are retained in the
`beam-parity-<sha>` workflow artifact.

## Highest-priority runtime bottleneck: idle actor footprint

`Actor::new` currently creates a 16 KiB initial ORCA heap for every actor even
though `ActorHeap` already grows by chaining additional bump blocks. That
initial reservation alone is about 16 GiB for one million resident actors,
before the `Actor` struct, mailbox, maps, vectors, flight recorder, scheduler
queues, allocator metadata, or live state are counted.

The first density optimization should therefore target the initial heap rather
than scheduler micro-optimizations.

Candidate implementation order:

1. Measure current spawn throughput and resident memory at 10k/100k actors.
2. Reduce the initial actor bump block to a BEAM-like small size (start with
   2 KiB) and rely on the existing chained-growth path.
3. If allocator overhead remains material, make the first bump block lazy so
   an actor that never heap-allocates owns no bump block at all.
4. Keep the main/top-level VM heap separate; this optimization is specifically
   for actor density.
5. Re-run message-heavy workloads to quantify the cost of additional growth
   allocations and choose the smallest initial size that does not materially
   regress normal actors.

## Same-host actor-density A/B gate

Actor-density PR branches (`perf/actor-density*`) run an additional CI job that
compares the PR base and candidate on the **same GitHub Actions runner**. This
avoids treating absolute shared-runner measurements as stable across unrelated
jobs.

The gate builds both revisions in release mode and runs the density probe at
10k and 100k resident actors. It records:

- RSS delta and RSS bytes per idle actor;
- initial actor-heap capacity per actor;
- actor spawn throughput;
- one-message-per-actor fan-out throughput;
- shutdown/reclamation time and post-shutdown RSS.

The candidate must reduce initial actor heap capacity to at most 25% of the
base and improve RSS/actor by at least 25%. Spawn and fan-out throughput each
have a 25% regression budget to absorb shared-runner noise; crossing that
budget fails the job and requires investigation rather than silently accepting
the memory/throughput trade.

The raw base/candidate outputs and comparison summary are uploaded as the
`actor-density-<pr>-<sha>` workflow artifact. These CI results are a merge
gate for density changes, not a substitute for the fixed-host 500k/1M ladder.

## Manual density ladder

Density tests should not run on every shared CI push. Run them deliberately on
a fixed host and record both elapsed time and resident memory. The manual probe
is `examples/actor_density.rs`:

```bash
cargo run --release --example actor_density -- 10000
cargo run --release --example actor_density -- 100000
cargo run --release --example actor_density -- 500000
cargo run --release --example actor_density -- 1000000
```

On Linux it reports RSS before spawning, RSS after spawning, RSS after the
scheduler settles, spawn throughput, scheduler-settle time, and initial actor
heap capacity. On other platforms the runtime metrics still work but RSS is
reported as unavailable.

| Stage | Resident actors | Purpose |
|---|---:|---|
| A | 10,000 | fast development check |
| B | 100,000 | allocator/scheduler scaling |
| C | 500,000 | memory-pressure behavior |
| D | 1,000,000 | BEAM-class density target |

For each stage record:

- wall-clock actor creation time;
- peak RSS and virtual memory;
- bytes per idle actor (`(RSS_after - RSS_before) / actor_count`);
- one-message-per-actor fan-out throughput and drain time;
- p50/p95/p99 end-to-end message latency under sustained load (separate harness);
- shutdown/reclamation time and residual RSS.

## Durable-actor benchmark track

Nulang should not optimize only for BEAM's workload. Its intended advantage is
that actors can also be durable entities/workflows. After the ordinary actor
baseline is stable, add a second track that BEAM does not provide natively:

1. create 100k actors, with 10k durable;
2. send and checkpoint state changes;
3. terminate the runtime abruptly;
4. recover all durable actors;
5. verify actor identity, state, authority and message/journal position;
6. resume suspended workflows;
7. repeat after a schema upgrade that requires a deterministic migration.

Measure recovery throughput, checkpoint amplification, storage bytes per actor,
and time-to-first-useful-message after restart.

## Acceptance criteria

Do not call Nulang "BEAM parity" until the evidence supports all of the
following on the same hardware:

- one million idle actors are practical without pathological memory pressure;
- scheduler throughput scales without catastrophic tail-latency collapse;
- selective receive/mailbox growth remains bounded and observable;
- supervision/restart storms preserve correctness under load;
- distributed messaging has explicit backpressure and no silent loss;
- durable recovery is deterministic across crash/restart/version-upgrade paths.

Nulang does not need to beat BEAM on every conventional actor benchmark. The
strategic target is to get close enough on ordinary actor mechanics that its
stronger durability, effect, capability and agent semantics become the deciding
advantage rather than being offset by runtime overhead.
