# Deterministic scheduler schedule traces (experimental)

Nulang's real-runtime deterministic scheduler already accepts a seed and step
budget, supports virtual clocks, and executes real actor behavior through
`Runtime::step_actor`. The additional recorded-schedule API captures which
ready actor was selected at each step and when the virtual clock was advanced.

## Rust test harness

1. Construct a fresh `Runtime`, install a virtual clock, and register actors
   and their initial messages.
2. Call `run_scheduler_deterministic_recorded(seed, max_steps)`.
3. Save the returned `DeterministicScheduleTrace` using `serde_json`. It includes
   the seed, budget, event sequence, and terminal outcome.
4. Reconstruct the *same* initial workload on a **fresh** runtime and call
   `verify_deterministic_trace(&recorded_trace)`. A mismatched scheduling
   decision, step count, or terminal result returns
   `DeterministicTraceMismatch`.
5. Assert domain-specific state invariants separately: a matching schedule alone
   does **not** prove identical application states.

Actor steps store the selected **rank in the sorted ready actor IDs** and the
number of ready actors, not process-global actor IDs. That lets a new runtime
reproduce a schedule even if it allocates different numeric IDs.

Timer advances are recorded as logical events rather than nanosecond durations.
The current timer wheel computes deadlines from `Instant::now()`, so raw
deadline deltas cannot be assumed identical across separately initialized
runtimes. This trace proves event order and step count, **not** an exact
wall-clock timeline.

## Scope and limitations

- This is seeded **re-execution and verification**, not a forced scheduler
  trace interpreter. It detects divergence *after* a bounded run finishes.
- Install a virtual clock for timer-bearing workloads and control all sources
  of external nondeterminism. Test handlers, fakes, or persisted idempotent
  receipts are required for effects that would otherwise mutate the world.
- A trace does not capture message contents, actor state, network delivery,
  storage history, or side effects. Recording is not automatically enabled
  in the production scheduler and does not change its scheduling policy.
- The selected-ready rank assumes equivalent actor creation order within
  compared runs; actor ID values themselves can differ.
- For distributed tests, use `cluster_dst::DeterministicCluster` (test-only)
  and `dst::shrink::minimize_failing_trace`. Cross-node packet/partition replay
  is not yet integrated into this single-runtime schedule record.
- Keep `max_steps` bounded and only verify traces against independent,
  equivalently initialized runtimes. A nonmatching trace does not roll back
  execution.

## Regression gate

```bash
cargo test --locked dst_trace_tests
cargo test --locked
cargo fmt --all -- --check
```

Next validation: use a real actor program with message-driven timer creation
and durable-effect fakes, then reproduce its fault across fresh runtimes with
controlled clock origins. Only after that should the harness claim complete
clock/fault/event replay.
