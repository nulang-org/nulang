# Deterministic scheduler traces (experimental)

The deterministic test scheduler executes real Nulang actor behaviors using a
seed and a bounded step budget. Its recorded trace stores the ready-set rank of
each selected actor, logical timer-advance events, and the termination result.
The trace does not record an actor snapshot, external effects, or wire traffic.

## Recording a regression trace

On a freshly initialized Rust `Runtime`, install a virtual clock, register
actors and initial messages, then call:

```rust
let trace = runtime.run_scheduler_deterministic_recorded(42, 1000);
let json = serde_json::to_string_pretty(&trace)?;
```

Use the trace with an **independently rebuilt, equivalent** runtime. There
are two different checks:

- `verify_deterministic_trace(&trace)`: re-executes with the same seed,
  records its choices, then compares the two traces **after** the run.
- `replay_deterministic_trace(&trace)`: **forces** the recorded actor selection
  by rank within the sorted ready set. It validates the ready-set size,
  selected index, and timer-advance events **before** each step, rejecting the
  first missing or incompatible event. It also checks the terminal outcome and
  extra trace events.

The forced-replay path does **not** redraw choices from the seed; the seed is
still carried as provenance in JSON. Both paths use real actor execution. Both
require controlled effects, and neither rolls back matching steps already
executed before divergence.

Actor identifiers are process-global, so the trace intentionally uses sorted
ready-set ranks rather than raw actor IDs. Different numeric IDs are fine if
the relative actor creation order, behavior, messages, and injected effects are
equivalent. The harness must separately assert application state, durable
receipts, and invariants; a matching schedule is not a state-equivalence proof.

## Timer clock boundary

The standalone `TimerWheel` convenience APIs still schedule relative to
`Instant::now()`, preserving existing real-time callers. Its explicit
clock-relative APIs (`send_after_at`, `send_after_with_context_at`,
`receive_wait_timeout_at`, `schedule_llm_retry_at`, `remaining_at`, and
other corresponding `*_at` methods) use the caller's `Instant`.

The runtime's workflow timer re-arming, receive timeouts, actor timer callbacks,
and LLM retry scheduling now pass `Runtime::now()`. These must remain
clock-consistent with the runtime's `tick_timers_at` and virtual-clock
advancement. Equal-deadline timers fire in ascending timer-ID order.

Traces record logical timer advances, **not** raw `Instant` values or elapsed
nanoseconds. Each fresh test runtime has a distinct virtual-clock epoch.
Tests should schedule through the runtime clock after advancing it and should
not mix standalone real-time timer-wheel methods with virtual-time programs.

## Safety boundaries and future work

- These APIs are a **test harness**, not a live production scheduler or a
  persisted durable actor-history format.
- No network packets, external side effects, nondeterministic RNG results,
  persistence snapshots, or process-kill outcomes are captured in these traces.
  Use deterministic providers and fault-injection fixtures for such tests.
- Forced replay detects mismatch before an actor step or timer advance, but
  network draining and already due timer delivery may happen before mismatch
  detection, and previously matched effects cannot be reversed.
- `max_steps` caps actor turns plus logical timer advances. A step-limit result
  is a bounded-test outcome, **not** by itself proof of deadlock.
- `cluster_dst::DeterministicCluster` and `dst::shrink` remain the mechanism
  for inter-node network faults, partitions, hard crashes, and trace shrinking;
  those fault schedules are not yet integrated with this actor-choice trace.
- A next hardening step is unified event/fault replay with receipt-backed
  provider failure injection and explicit clock epochs. Keep it separate from
  live runtime semantics until multi-backend conformance is established.

## Verification gate

```bash
cargo fmt --all -- --check
cargo test --locked dst_trace_tests
cargo test --locked runtime::timer::tests
cargo test --locked
cargo check --all-targets
cargo check --all-targets --no-default-features
```

Run these on the exact PR head in a working Rust environment. CI workflows with
zero jobs or skipped correctness jobs do not constitute verification.
