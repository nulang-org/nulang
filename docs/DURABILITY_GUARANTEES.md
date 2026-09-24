# Durable Execution Guarantees

This document defines the claim boundary for Nulang durable external effects.
It is intentionally narrower than "exactly once".

The executable contract spans:

- `src/semantic_identity.rs` — compiler-owned static effect-site identity;
- `src/durable_effect.rs` — effect state machine and delivery semantics;
- `src/durable_effect_runtime.rs` — storage-neutral prepare/recover/complete coordinator;
- `src/runtime/persistence.rs` — atomic transitions, activation fencing, and recovery lookup;
- `src/durable_effect_persistence.rs` — versioned persisted effect records.

The end-to-end deterministic release gate is
`tests/durable_effect_failure_matrix.rs`.

## What Nulang can guarantee locally

For one logical durable external effect, the runtime contract is:

1. derive the static call site from compiler-owned semantic identity;
2. combine it with replay-stable execution identity to derive one `DurableEffectId`;
3. persist a `Prepared` record atomically before provider execution;
4. bind the record to the exact request digest and full effect specification;
5. reuse the same operation ID on every recovery attempt;
6. persist the first terminal result as `Completed`;
7. replay a completed result without redispatching the provider;
8. reject request or specification drift before dispatch;
9. reject stale activation epochs at the persistence boundary;
10. model compensation as another explicit durable effect rather than pretending
    the original external mutation can be rolled back.

A Nulang-local journal or transaction cannot prove that an arbitrary remote
system executed an operation exactly once. A process can always fail after the
remote side commits and before Nulang records the receipt.

## Crash-window matrix

| Crash point | Durable state after restart | Required recovery | External observation |
|---|---|---|---|
| Before intent commit | No effect record | Start a logical attempt only if the owning durable turn was not committed | No provider call should have occurred |
| After intent, before provider | `Prepared` | Retry the same semantic operation ID | One call if the first process never reached the provider |
| After provider commit, before receipt | `Prepared` | Retry with the same operation ID | May duplicate under at-least-once; effectively-once requires provider/backend deduplication |
| After receipt, before actor/workflow resume | `Completed` | Return the recorded result | Provider must not be called again |
| Replay request/spec differs | `Prepared` or `Completed` | Fail closed | Provider must not be called |
| Backend owns semantics | Backend-defined | Delegate to the backend contract | Nulang must not strengthen the guarantee |

## Delivery semantics

### `AtLeastOnce`

Recovery may execute the dependency again. Duplicate external mutation is
permitted by the contract. Callers must use an idempotent operation, explicit
deduplication, or accept duplicates.

### `EffectivelyOnceWithDeduplication`

Nulang retries with the exact same stable operation ID. The external dependency
must actually honor that key (or provide an equivalent query/dedup mechanism).
Nulang can then observe one logical outcome across retries, but this is not a
blanket exactly-once guarantee for an uncooperative provider.

### `BackendDefined`

The configured backend owns retry, deduplication, and commit semantics. Nulang
delegates rather than inferring a stronger guarantee.

## What is not yet a production-wide guarantee

Passing this gate does **not** prove that every `perform`, workflow activity,
LLM call, HTTP request, or actor turn is wired through the durable coordinator.
Each adapter is production-ready only after its dispatch path consumes the
compiler-produced effect-site metadata and uses the same persisted operation ID.

A production external-effect adapter should have all of the following:

- compiler/runtime-stable semantic effect-site identity;
- validated bytecode/NBC effect-site metadata;
- intent persistence before dispatch;
- request and specification validation on recovery;
- a stable provider idempotency/deduplication key where required;
- a terminal receipt that replays without redispatch;
- activation fencing so stale owners cannot publish late results;
- atomic integration with the owning actor/workflow turn where its result
  affects durable state or inbox deduplication;
- trace metadata containing effect boundary, delivery semantics, and stable
  operation identity;
- deterministic crash-window tests;
- provider-specific timeout, throttling, response-loss, and failover tests.

## Release gate

Run:

```bash
bash scripts/test-durability-guarantees.sh
```

The gate is deterministic and performs no public network calls, sleeps, or
wall-clock assertions. It covers semantic-site artifact propagation, storage
recovery, crash-window behavior, and the lower-level durable-effect contracts.

## Competitive benchmark contract

Correctness comes before throughput. Once the same production adapter path uses
this contract, benchmark Nulang against durable-workflow systems with an
equivalent workload:

1. start a durable job;
2. perform a provider mutation with an idempotency key;
3. kill the process after provider commit but before local receipt commit;
4. restart on a fresh process;
5. complete the job;
6. assert external mutation count, recovered result, time to recovery, and
   steady-state overhead.

Report semantics separately from speed. A system that completes faster by
allowing duplicate mutation is not equivalent to one that closes the crash
window through deduplication.
