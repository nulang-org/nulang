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

## Output schema

Each JSONL row contains:

- `schema`: schema version, currently `1`
- `runtime`: `nulang`
- `suite`: `savina-style`
- `benchmark`: workload name
- `iteration`: one-based repetition number
- `messages`: logical messages counted by the workload
- `elapsed_ns`: timed wall-clock duration
- `messages_per_second`
- `ns_per_message`

Compilation, parsing, type checking, HIR/MIR lowering, and bytecode generation
happen before the timed region for each workload. Semantic assertions still
run after each measurement, so a fast but incorrect execution fails the run.

## Interpretation

These are Savina-*style* workloads, not yet canonical cross-language Savina
results. In particular, Nulang's current Skynet case is depth 3 (1,111 actors)
rather than the canonical million-leaf scale because the current per-actor heap
makes that scale impractical. Cross-language claims should use matched workload
sizes and identical hardware, and should report several repetitions rather than
a single shared-runner sample.
