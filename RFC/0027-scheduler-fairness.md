# RFC 0027: Weighted-Fair Actor Priority Scheduling

- **Status:** Draft — experimental, opt-in scheduler integration; no production default change
- **Tier:** Experimental
- **Created:** 2026-09-16

## Summary

Nulang's production actor scheduler probes priority bands in strict order on every dequeue:

```text
High -> Normal -> Low
```

That is simple and minimizes High-priority latency, but a continuously non-empty High queue can starve Normal and Low work indefinitely.

This RFC introduces a deterministic weighted service policy. The *proposed fair configuration*, which is not Nulang's runtime default, is:

```text
High:   8 preferred turns
Normal: 4 preferred turns
Low:    1 preferred turn
```

Each scheduling turn still probes all three bands. The weighted wheel changes only which band is tried first.

## Why not simple aging timestamps

Aging based on wall-clock wait time introduces more mutable per-task metadata, extra clock reads in the scheduler hot path, and nondeterministic behavior under replay/tests.

A deterministic service wheel provides a stronger initial property with less state:

- lower bands receive bounded preferred opportunities;
- the policy is independent of wall time;
- the sequence is reproducible in tests;
- High remains the dominant class;
- empty preferred bands immediately fall back to other work.

A future aging layer can still be added for latency SLOs that need time-based guarantees.

## Semantics

For a preferred High turn:

```text
High -> Normal -> Low
```

For a preferred Normal turn:

```text
Normal -> High -> Low
```

For a preferred Low turn:

```text
Low -> High -> Normal
```

With default 8:4:1 weights and all queues continuously runnable, the maximum wait for a Low preferred opportunity is one 13-turn cycle instead of unbounded starvation.

This is a service-opportunity bound, not a wall-clock latency bound. Actor turn length and reduction quotas remain separate concerns.

## System-message priority is separate

Mailbox `System` messages bypass bounded admission, and the priority-lane contract requires system traffic ahead of ordinary application traffic. Separate mailbox credit-accounting corrections are under review; this proposal does not change their behavior. This RFC concerns **actor scheduling priority** (`High`, `Normal`, `Low`), not supervisor/system-message ordering inside an actor mailbox.

Therefore weighted actor scheduling does not weaken supervision signal priority.

## Compatibility mode

The policy module also exposes a strict High-first policy equivalent to current behavior:

```text
weights = 1:0:0
```

Normal and Low remain fallback bands when High is empty, but receive no preferred slots.

This allows benchmark/regression comparisons and an opt-out for workloads that explicitly require strict actor priority.

## Integration plan and current scope

1. The policy primitive is independent of Chase-Lev queues and has deterministic tests.
2. `Scheduler::new(worker_count)` **remains strict priority**. An embedding Rust caller can select the opt-in `Scheduler::new_with_fair_priority(worker_count, weights)` constructor, which validates nonzero weights.
3. `next_task`, `dequeue` (owner path), and `steal_one` obtain a single atomic turn ticket per attempt. Each turn uses the pure policy lookup. A retry inside a queue's steal operation does not consume another ticket.
4. Scheduler-level saturated-queue tests exercise the owner, generic worker, and steal paths. These are compile-time tests awaiting exact-head CI, **not yet passing performance evidence**.
5. **Before making any runtime default change:** measure throughput, High-priority p50/p99 latency, queue residence time, multi-worker contention/steal behavior, and scheduling overhead on the exact head. Use same-runner strict-vs-fair comparison and test both large and small actor turns.
6. Expose fairness weights and per-band service counters through introspection only if the policy is adopted.

The global atomic turn ticket coordinates logical attempts on a shared scheduler, not per-actor execution completion. Under concurrency, a reserved preferred slot is an opportunity, not a guarantee of dispatch timing or a wall-clock bound.

## Controlled strict-vs-fair scheduler probe

An opt-in, ignored Rust test supplies an identical 30,000-token fixture
(10,000 High, Normal and Low tokens) to strict and explicitly fair scheduler
instances. It measures the scheduler owner's dequeue loop, alternating the
execution order across six measured rounds after one warm-up:

```bash
cargo test --locked --release bench_scheduler_strict_vs_fair -- --ignored --nocapture
```

Each round fails if there are duplicate, missing or extra task tokens and emits
`[scheduler-fair-bench]` machine-readable lines containing the mode,
operation count, loop elapsed time and High-priority p99 queue waiting time.
The `[scheduler-fair-summary]` line reports sample medians. Benchmarking
requires the same host, CPU affinity, toolchain and exact source revision.

**Limits:** This is a synthetic, preloaded, single-owner, payload-free test.
The High p99 is **enqueue-to-dequeue queue delay**, including fixture enqueue
time; it is not actor-handler p99, asynchronous wakeup p99 or full end-to-end
latency. Per-iteration bookkeeping also contributes to measured throughput.
This microbenchmark is not a production-default adoption gate by itself.
Follow with mixed live-actor workloads, sustained producer pressure, and
multi-worker contention before deciding whether to enable the policy.

## Operational defaults

The proposed fair-mode starting weights are `8:4:1`; the deployed default remains **strict High-first** pending performance and semantics validation. The proposed weights are benchmark-tunable, not language semantics.

The language should not expose these exact weights as source-level semantics. They are runtime scheduling policy.

## Non-goals

This RFC does not introduce actor preemption, concurrent actor turns, mailbox reordering, or wall-clock deadlines. One actor still processes one turn at a time unless a separate controlled-reentrancy feature is explicitly enabled.
