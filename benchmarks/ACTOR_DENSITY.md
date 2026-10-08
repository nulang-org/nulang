# Actor Density Measurement

This track measures the runtime property that remains weak in the current
Criterion snapshot: actor creation and residency. It is measurement-only
infrastructure and changes no runtime semantics. It is deliberately separate
from the default `cargo bench` job because 100k-1M actor measurements require
a fixed, otherwise-idle host and are not useful when repeated on shared CI.

## Current state

Current `main` already avoids the two largest historical eager allocations:

- an `ActorHeap` does not allocate its configured first bump block until the
  first small-object allocation;
- runtime flight recording is opt-in and its backing vector starts empty.

Do not infer committed heap bytes from `ActorHeap::free_bytes()`: before the
first allocation it describes configured capacity, not resident memory.

The current rolling Criterion snapshot reports roughly 35.8 ms for
`actor/spawn_idle/1000`, so the next question is which remaining component
dominates creation and residency.

## Nulang probe

Run release builds on a controlled host:

```bash
cargo run --release --example actor_density -- idle 10000
cargo run --release --example actor_density -- idle 100000
cargo run --release --example actor_density -- idle 500000
cargo run --release --example actor_density -- idle 1000000
```

The `idle` mode reports:

- fixed Rust layout sizes for `Actor`, `Mailbox`, `ActorHeap`, `OrcaGc`,
  `FlightRecorder`, and `TraceEntry`;
- full `Runtime::spawn_actor` throughput;
- scheduler settle time;
- actor-heap bytes actually used by live objects;
- Linux RSS delta and RSS delta per actor.

Use `construct` to isolate raw `Actor::new` construction from runtime table
insertion and scheduler publication:

```bash
cargo run --release --example actor_density -- construct 100000
```

Use `fanout` to measure one message to each already-created actor:

```bash
cargo run --release --example actor_density -- fanout 100000
```

Run modes in separate processes. Allocators may retain freed pages, so running
`construct` and `idle` sequentially in the same process would contaminate
RSS comparisons.

## Exact-base Nulang A/B

For a candidate performance branch, prefer the paired exact-base comparator to
independent before/after runs:

```bash
python3 scripts/actor_density_ab_bench.py \
  --base-ref <exact-parent-sha> \
  --actors 10000 \
  --runs 5 \
  --warmup 1 \
  --cpu-mode single \
  --no-default-features \
  --output actor-density-ab.json
```

The comparator builds the base and candidate into separate Cargo target
directories, then runs `construct` and `idle` in fresh processes. Measurement
rounds counterbalance base/candidate and mode ordering so monotonic thermal or
host-load drift is less likely to correlate with one revision. Reported
comparisons include paired construction/spawn latency, RSS delta per actor when
Linux reports a positive measurable delta for every pair, and the exact fixed
`Actor` layout change.

A zero or unavailable RSS delta is treated as an unavailable RSS comparison,
not as an infinite improvement/regression. Timing and fixed layout evidence
remain valid in that case.

The `Actor density A/B` workflow uses the same harness for actor
representation/spawn PRs. Its shared-runner output is useful same-host A/B
evidence, but publication-quality 100k-1M residency claims still belong on a
controlled otherwise-idle host using the density ladder below.

## Same-host BEAM comparator

`benchmarks/beam/actor_density.escript` provides matching spawn and fan-out
controls:

```bash
escript benchmarks/beam/actor_density.escript 10000 100000
```

Do not compare a local BEAM result against GitHub-hosted Nulang numbers. Run
both runtimes on the same machine, pin them to the same CPU set when comparing
single-core work, and record toolchain/OTP/Rust versions.

## Density ladder

| Stage | Actors | Purpose |
|---|---:|---|
| A | 10,000 | fast development check |
| B | 100,000 | allocator/runtime-table scaling |
| C | 500,000 | memory-pressure behavior |
| D | 1,000,000 | density target |

At each stage record spawn/sec, RSS delta, bytes/actor, scheduler settle time,
fan-out throughput, shutdown time, CPU model, core count, allocator/runtime
configuration, Rust version, and OTP version when using BEAM.

## How to act on the result

- If `construct` and `idle` are close in ns/actor, optimize `Actor::new`
  and fixed in-struct state first.
- If `idle` is materially slower than `construct`, profile runtime map
  insertion, scheduler publication, CRDT hooks, and generated actor-name
  allocation before changing actor semantics.
- If bytes/actor are dominated by `size_of::<Actor>()`, move rare state
  behind lazy optional extensions rather than micro-optimizing empty
  collections.
- If fan-out degrades before memory becomes limiting, investigate scheduling
  and active-set locality separately from dormant density.
- Do not reduce heap block sizes merely to improve idle numbers: the first
  block is already lazy, so block size matters only after activation.

Issue #284 tracks the implementation decisions that should follow this
measurement.
