# Savina-style actor benchmarks

Nulang has a standalone runner for five actor workloads modeled after the
Savina benchmark suite: counting, ping-pong, thread-ring, fork-join, and
Skynet.

The runner is deliberately separate from the full test suite. It builds only
the core compiler/runtime path and uses a custom optimized profile with LTO
disabled, which makes iteration substantially cheaper without changing the
existing Criterion benchmark profile or its historical baselines.

## Run

```bash
cargo run --locked \
  --profile savina \
  --no-default-features \
  --features savina-bench \
  --bin nulang-savina \
  -- --format human
```

For machine-readable output:

```bash
cargo run --locked \
  --profile savina \
  --no-default-features \
  --features savina-bench \
  --bin nulang-savina \
  -- --format jsonl --repeat 5
```

Run one workload with `--benchmark counting` (or `ping_pong`,
`thread_ring`, `fork_join`, `skynet`). Use `--list` to print the
supported names.

### Sharded fork-join

The default remains one shard. To exercise Nulang's real thread-per-shard
runtime on the fork-join workload, opt in explicitly:

```bash
cargo run --locked \
  --profile savina \
  --no-default-features \
  --features savina-bench \
  --bin nulang-savina \
  -- --benchmark fork_join --shards 4 --format human
```

Only `fork_join` currently has a sharded fixture; counting, ping-pong,
thread-ring, and Skynet remain single-shard even when `--shards` is supplied.
The sharded fixture places eight bytecode workers round-robin across 2–8 real
`Runtime` shards. OS threads are created before timing and released by a
barrier. The producer keeps at most 512 tasks in flight, below the 1024-entry
cross-shard channel capacity, so a large preloaded burst cannot silently turn
channel backpressure into dropped benchmark work.

## Output schema

Each JSONL row contains:

- `schema`: schema version, currently `2`
- `runtime`: `nulang`
- `suite`: `savina-style`
- `benchmark`: workload name
- `iteration`: one-based repetition number
- `messages`: logical messages counted by the workload
- `elapsed_ns`: timed wall-clock duration
- `messages_per_second`
- `ns_per_message`
- `shards`: real `Runtime` shards used by that measurement

Compilation, parsing, type checking, HIR/MIR lowering, and bytecode generation
happen before the timed region for each workload. Semantic assertions still
run after each measurement, so a fast but incorrect execution fails the run.

## Interpretation

These are Savina-*style* workloads, not yet canonical cross-language Savina
results. In particular, Nulang's current Skynet case is depth 3 (1,111 actors)
rather than the canonical million-leaf scale because the current per-actor heap
makes that scale impractical. The sharded fork-join fixture preserves the same
50,000 tasks / 100,000 logical-message accounting as the single-shard case, but
its bounded in-flight producer differs from the historical preloaded single-
shard producer. Use it for multicore scaling and same-host runtime comparisons,
not as evidence that scheduling semantics are identical across languages.

Cross-language claims should use matched workload sizes and identical hardware,
record the shard count, and report several repetitions rather than a single
shared-runner sample.
