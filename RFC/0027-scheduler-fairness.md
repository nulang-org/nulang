# RFC 0027: Weighted-Fair Actor Priority Scheduling

- **Status:** Draft — policy primitive implemented
- **Tier:** Experimental
- **Created:** 2026-09-16

## Summary

Nulang's current actor scheduler probes priority bands in strict order on every dequeue:

```text
High -> Normal -> Low
```

That is simple and minimizes High-priority latency, but a continuously non-empty High queue can starve Normal and Low work indefinitely.

This RFC introduces a deterministic weighted service policy. The default cycle is:

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

Mailbox `System` messages already bypass application mailbox capacity and are processed ahead of normal mailbox traffic. This RFC concerns **actor scheduling priority** (`High`, `Normal`, `Low`), not supervisor/system-message ordering inside an actor mailbox.

Therefore weighted actor scheduling does not weaken supervision signal priority.

## Compatibility mode

The policy module also exposes a strict High-first policy equivalent to current behavior:

```text
weights = 1:0:0
```

Normal and Low remain fallback bands when High is empty, but receive no preferred slots.

This allows benchmark/regression comparisons and an opt-out for workloads that explicitly require strict actor priority.

## Integration plan

1. Keep `FairPriorityPolicy` independent of Chase-Lev queues and benchmark it first.
2. Add one policy instance per scheduler owner/worker or shard.
3. Replace hard-coded `[High, Normal, Low]` probe arrays in `next_task`, `next_owner_task`, and `steal_one` with the policy's per-turn order.
4. Ensure one logical dequeue attempt advances the wheel once; retries inside a queue steal operation must not advance policy state.
5. Add saturated-queue tests showing Normal and Low make progress while High remains dominant.
6. Add benchmarks for throughput, p50/p99 High latency, queue residence time, steal behavior, and cache effects.
7. Expose fairness weights and per-band service counters through actor/scheduler introspection.

## Operational defaults

The proposed initial default is `8:4:1`. It is intentionally conservative and should be treated as benchmark-tunable rather than language semantics.

The language should not expose these exact weights as source-level semantics. They are runtime scheduling policy.

## Non-goals

This RFC does not introduce actor preemption, concurrent actor turns, mailbox reordering, or wall-clock deadlines. One actor still processes one turn at a time unless a separate controlled-reentrancy feature is explicitly enabled.
