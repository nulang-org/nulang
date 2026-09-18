# RFC 0029: Controlled Actor Interleaving

- **Status:** Draft — policy primitive implemented
- **Tier:** Experimental
- **Created:** 2026-09-16
- **Depends on:** RFC 0017 unified actor runtime

## Summary

Nulang should keep serial actor execution as its default and add only narrowly controlled interleaving where the compiler/runtime can establish safety.

The current runtime deliberately refuses to process new mailbox messages while an actor has a live suspension. That behavior is simple, deterministic, and friendly to durable replay. General actor reentrancy would weaken those properties by exposing intermediate state and introducing timing-dependent interleavings.

This RFC therefore does **not** add a blanket `reentrant` actor flag.

The first permissible extension is:

> A behavior proven read-only and replay-safe may run while an exclusive behavior is suspended, but only when that specific suspension point explicitly permits read-only interleaving.

## Default

```text
ActorInterleavingPolicy::serial()
```

Serial remains the default and preserves today's behavior.

## Behavior classification

```text
Exclusive
ReadOnly
```

`Exclusive` is the default for any behavior that may mutate actor state or perform effects whose replay/interleaving semantics are not proven safe.

`ReadOnly` must eventually be a compiler-produced classification. It must not merely be a user assertion. At minimum the compiler should verify:

- no actor state mutation;
- no mutable alias escape;
- no non-replay-safe effects;
- no writes through external storage handles;
- no capability operation whose result changes actor-visible state during the interleaving window.

## Suspension classification

```text
BlockAll
AllowReadOnly
```

Ordinary suspension remains `BlockAll`.

`AllowReadOnly` is an explicit opt-in associated with the suspended turn/suspension point. It allows only compiler-verified read-only turns behind it.

A running, non-suspended exclusive turn never interleaves.

## Admission rules

For an actor with read-only interleaving enabled:

| Current actor state | Incoming exclusive | Incoming read-only |
|---|---|---|
| idle | admit | admit |
| exclusive running | deny | deny |
| exclusive suspended / BlockAll | deny | deny |
| exclusive suspended / AllowReadOnly | deny | admit up to limit |
| read-only turns active | deny | admit up to limit |

The read-only in-flight limit is explicit and bounded.

## Why not general reentrancy

General reentrancy creates several hazards for Nulang's model:

1. **Intermediate state visibility.** A suspended behavior may have partially updated state before awaiting.
2. **Replay nondeterminism.** Recovery may observe a different interleaving order from the original execution.
3. **Effect duplication/order changes.** Durable effects can move relative to nested messages.
4. **Invariant fragmentation.** Actor code can no longer reason that one behavior owns mutable state until completion/suspension resolution.
5. **Backend divergence.** Interpreter, AOT, WASM, and distributed execution could accidentally implement different reentrancy details.

For these reasons, same-correlation reentrancy, arbitrary concurrency groups, and concurrent mutable behaviors remain out of scope until Nulang has a stronger state/effect isolation model.

## Durable actors

Durable actors require a stricter rule than transient actors: read-only interleaving must not change the durable journal outcome or introduce a replay-visible mutation.

A future compiler certification should be part of the artifact metadata and checked again by the runtime rather than inferred from behavior names or annotations alone.

## Relationship to queries

Read-only interleaving is especially useful for operational/query behaviors such as:

```text
status()
progress()
health()
current_view()
```

A durable workflow waiting on a timer, signal, or remote effect can answer these without letting a second mutating command enter its state machine.

## Implemented primitive

`src/actor_interleaving.rs` provides:

- `BehaviorConcurrency::{Exclusive, ReadOnly}`;
- `SuspensionInterleave::{BlockAll, AllowReadOnly}`;
- `ActorInterleavingPolicy`, with serial as the default;
- bounded read-only admission;
- scheduler-visible `TurnAdmissionState`;
- deterministic admission decisions;
- tests proving serial compatibility, safe-suspension-only admission, reader limits, exclusive exclusion, and BlockAll behavior.

The module is additive and does not alter the live scheduler yet.

## Follow-up

1. Add compiler proof/metadata for read-only behaviors.
2. Mark specific runtime suspension classes eligible for `AllowReadOnly` only where replay semantics are defined.
3. Add actor runtime counters for active read-only turns.
4. Integrate the admission policy with `step_actor` while preserving the serial default.
5. Add deterministic replay tests containing interleaved queries.
6. Benchmark query latency and scheduler overhead.
7. Consider richer concurrency groups only after state partitioning/isolation exists.

## Decision

Nulang favors deterministic actor isolation over maximum concurrency. Controlled read-only interleaving is an optimization layer on top of the serial actor model, not a replacement for it.
