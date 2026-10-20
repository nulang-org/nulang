# Experimental Dataflow Reference Prototype

**Status:** Rust-hosted reference implementation only. No new Nulang syntax,
MIR passes, actor scheduling, Fabric delivery contract, or persistent format.
Not yet a streaming engine.

## Scope

`src/dataflow.rs` implements a finite, homogeneous typed DAG:

- `source(Vec<T>)`, `map`, `filter`, `fold`, `concat`
- Append-only construction: edges must reference existing nodes; cycles and
  forward references cannot be constructed through the public builder.
- Evaluate one chosen output and its transitive predecessors in insertion
  (topological) order. Unrelated nodes are not evaluated.
- Shared predecessors are materialized once **per execution call**.
- `fold` returns a one-element collection (the accumulator), including when
  its input is empty. `concat` preserves left-to-right sequence order.
- `Limits` rejects zero limits and enforces node count, output items per
  stage, and cumulative logical items materialized by one evaluation.
- `BoundedQueue<T>` has FIFO semantics. A full queue returns the submitted
  item with `Err(item)`; no data is silently discarded.

This is deliberately a **reference executor**, not a performance claim.
Operators are Rust functions with no static purity enforcement; callers must
supply deterministic, side-effect-free callbacks for deterministic results.
The Nulang type/effect/capability checker is **not** connected yet. Graph
operators currently use the same element type `T` throughout; cross-type
map stages require future typed IR design.

Item limits are logical element limits, not byte or resident-memory limits.
Source values also reside in the graph and execution may clone items. The
queue is not connected to the graph executor, any actor mailbox, or Fabric.
It is an admission-contract building block, not end-to-end backpressure.
It provides no cancellation, ACK/NACK, fairness, remote delivery, durable
checkpointing, idempotency or exactly-once guarantee. User-supplied callback
panics are not intercepted.

## Focused tests

```sh
cargo test --locked --lib dataflow::tests
cargo fmt --all --check
cargo clippy --locked --lib --all-targets --no-default-features -- -D warnings
```

The full repository CI matrix remains necessary before merge; this prototype
is not evidence that bytecode, WASM, or native backends implement dataflow.

## Follow-up gates (do not bypass)

1. Establish the canonical source-language pure-expression/effect/authority
   eligibility check; do not infer purity from Rust callback types.
2. Define an executor-neutral typed graph IR and prove observational parity
   with ordinary sequential Nulang evaluation on edge-case fixtures.
3. Benchmark operation fusion against existing MIR and SIMD optimizations.
   Avoid a competing MIR optimizer while related optimizer PRs are pending.
4. Connect bounded admission, upstream demand, cancellation, and failure
   propagation to the RFC 0024 scoped-task executor **after** its semantic
   contracts and pending PR stack are reconciled.
5. Keep Fabric integration separate until acknowledged delivery, retries,
   checkpoint/replay, schema and idempotency contracts are established.

See [RFC 0024](../RFC/0024-orthogonal-execution-model.md) for the orthogonal
execution model and [Fabric](FABRIC.md) for the Experimental stream boundary.
