# RFC 0025: Structured Concurrency Semantics

- **Status:** Proposed
- **Tier:** Experimental until implementation/conformance gates are complete
- **Created:** 2026-09-23
- **Depends on:** RFC 0024 (Orthogonal Execution Model)
- **Scope:** source-level `par`, scoped-task semantics, compiler legality checks, and runtime execution policy

## Summary

Nulang's `par { ... }` construct becomes the source-level surface for **scoped
structured concurrency**.

A `par` region is not an actor and does not create independently addressable
runtime identities. Its child computations:

1. are lexically bounded by the parent `par` scope;
2. cannot outlive that scope;
3. join deterministically;
4. obey reference-capability and ownership rules at capture boundaries;
5. use compiler-owned effect semantics to decide whether branches may execute
   concurrently;
6. preserve equivalent observable semantics on a sequential fallback backend;
7. never require making the normal actor-local ORCA fast path globally atomic.

This RFC intentionally specifies semantics before enabling parallel execution.
The current implementation may continue to run `par` branches sequentially
until the acceptance criteria below are met.

## Motivation

RFC 0024 separates three execution domains:

- local computation;
- scoped tasks;
- actors.

The existing `par` syntax is the natural surface for scoped tasks, but its
historical experimental behavior is only an independence annotation with block
semantics: branches execute in order and the final expression supplies the
result.

That behavior is a poor permanent contract for structured concurrency because:

- branch results are not first-class;
- there is no stable failure/cancellation rule;
- effect ordering is undefined once real concurrency is introduced;
- actor-local heap values cannot safely be sent to arbitrary worker threads;
- a backend could accidentally expose completion-order differences;
- compiler/runtime implementations could evolve incompatible notions of
  "parallel-safe".

Nulang already has the ingredients needed for a stronger model:

- typed HIR/MIR;
- reference capabilities;
- linear/ownership analysis;
- row-polymorphic effects;
- compiler-owned operation semantics;
- actor isolation and supervision;
- per-actor ORCA heaps;
- deterministic-testing infrastructure.

This RFC composes those pieces rather than introducing another independent
concurrency subsystem.

## 1. Source semantics

### 1.1 Result shape

A `par` region returns branch results in **source order**, never completion
order.

Target semantics:

```nulang
let (user, orders) = par {
    fetch_user(id)
    fetch_orders(id)
}
```

is conceptually equivalent to a typed tuple:

```text
(User, [Order])
```

even if `fetch_orders` completes first.

For N branches with result types `T0 ... Tn`, the result type is:

```text
(T0, T1, ... Tn)
```

A zero-branch `par {}` produces `Unit`.

A one-branch region produces a one-element tuple rather than collapsing to the
branch type. This keeps result shape structural and avoids arity-dependent type
rules.

Because `par` remains Experimental, this deliberately supersedes the current
"last expression wins" behavior before that behavior becomes a compatibility
obligation.

### 1.2 Scope

Every branch is a child of exactly one lexical task scope.

A child:

- cannot detach;
- cannot outlive the scope;
- cannot return/break/resume through the parent frame;
- cannot leave deferred cleanup pending after scope completion.

The parent does not leave the `par` expression until every started child has
either completed or reached the RFC-defined cancellation terminal state.

### 1.3 No implicit actor identity

A scoped task has no mailbox, process registry entry, stable identity,
supervision-tree node, remote address, or independent persistence identity.

Code requiring independently addressable isolated state must use an actor.

## 2. Deterministic join and failure

### 2.1 Join ordering

Successful results are assembled by source branch index.

Completion order is never observable through the `par` result value.

### 2.2 Failure policy

The default failure policy is **cancel siblings, then join cleanup**.

When one branch fails:

1. the task group records the failure;
2. sibling cancellation is requested;
3. all started siblings reach a cancellation/completion boundary;
4. cleanup completes;
5. the parent observes one failure.

If multiple branches fail before cancellation is observed, the visible failure
is selected by the lowest source branch index. Runtime scheduling order must not
change that choice.

This rule exists to keep sequential and concurrent hosts observationally
equivalent.

### 2.3 Cancellation is cooperative

Cancellation is not rollback.

A branch may be cancelled only at compiler/runtime-defined safe points.

Effects that have crossed an external commit boundary are not magically undone.
Their effect-operation semantics determine whether cancellation is immediate,
deferred, or unsupported without a protocol.

Compensation belongs to durable workflow/saga semantics, not ordinary scoped
tasks.

## 3. Capture model

Task capture safety is derived from Nulang's existing type and
reference-capability system.

The compiler should classify each captured value into one of four conceptual
classes.

### 3.1 Copy

Definitely scalar/non-owning values may be copied into worker execution.

Examples include integer, float, boolean, and other representations proven not
to contain actor-heap pointers.

### 3.2 FrozenShare

Immutable values may eventually be shared with concurrent workers only when the
runtime can prove that worker access will not mutate actor-local RC/GC metadata
or the underlying object.

Reference capability `val` is necessary evidence but is **not by itself
sufficient in the current runtime** because ORCA headers are actor/shard-affine
plain integers.

Until a safe pinned/frozen representation exists, these captures remain
actor-affine.

### 3.3 Move

Unique values such as `iso` / `lineariso` are candidates for ownership
transfer into exactly one task branch.

A move is legal only when existing compiler ownership analysis proves that the
source is consumed exactly as required.

The compiler must reuse the same ownership proof infrastructure used for local
last-use moves and consuming actor sends. Scoped tasks do not get a second,
incompatible ownership checker.

### 3.4 ActorAffine

Mutable/shared actor-heap values that cannot satisfy Copy/FrozenShare/Move stay
on the owning shard.

Examples include ordinary `ref`/`trn` mutable references and any value whose
runtime representation could require actor-local ORCA operations.

ActorAffine does not mean `par` is rejected. It means the group uses the
cooperative owning-shard executor unless a later proof removes that restriction.

## 4. ORCA and threading invariant

Current ORCA headers use plain integer reference counts. Actor heaps are
thread-confined by design.

Therefore, RFC 0025 explicitly rejects the implementation strategy:

```text
par branch -> arbitrary OS worker -> dereference/mutate actor heap
```

and also rejects making every ORCA operation atomic merely to enable scoped
parallelism.

The normal actor-local fast path must remain cheap.

True worker-thread execution is permitted only for branch state whose capture
classification proves that it does not require unsafe cross-thread actor-heap
access.

## 5. Effect-operation semantics

Structured concurrency consumes one compiler-owned effect-operation semantic
contract.

The minimum axes are:

- concurrency: Concurrent | Serialized | Unknown;
- replay: Deterministic | CaptureResult | RequiresProtocol | Unknown;
- idempotency: Idempotent | NonIdempotent | Unknown;
- cancellation: Immediate | Deferred | Unknown.

Unknown user-defined operations fail closed for worker-parallel scheduling.

A branch containing an Unknown or Serialized operation may still execute in a
`par` group through the sequential/cooperative executor.

This distinction is important:

```text
legal structured concurrency != eligible worker parallelism
```

The compiler must not duplicate effect allowlists independently in the task,
workflow, actor, and optimizer subsystems.

## 6. Execution modes

A `par` region has one semantic contract and multiple legal execution modes.

### 6.1 Sequential reference mode

Branches execute in source order.

This is the semantic reference fallback and must produce the same result/failure
selection as other modes.

### 6.2 Cooperative scoped mode

Branches are independently resumable but remain on the owning shard/thread.

This mode is appropriate for actor-affine values and suspending operations that
can overlap without violating actor heap ownership.

### 6.3 Worker-parallel mode

Branches execute concurrently on worker threads only when all required capture
and effect proofs succeed.

A backend is always allowed to fall back to cooperative or sequential execution
without changing language-visible semantics.

## 7. IR requirements

The current `ParallelRegionMarker::{Begin, Branch, End}` representation is a
valid transitional preservation mechanism.

Before worker execution, MIR should evolve toward an explicit structured region
representation that cannot be corrupted by ordinary CFG rewrites.

Conceptually:

```text
ParallelRegion {
    branches
    captures
    effect_summary
    result_shape
    failure_policy
}
```

Each branch should expose at least:

- source index;
- input captures;
- move/share classification;
- inferred effect summary;
- authority requirements;
- result type.

Markers may remain as a lowered form after the structured region has been
validated.

## 8. Authority

External authority and reference capability remain separate.

A task branch may request effects only within the authority already available to
its lexical parent.

A `par` region does not implicitly broaden or duplicate authority.

If future task APIs support explicit authority delegation, delegation must be
represented independently from reference-capability capture.

## 9. Actor state

Ordinary scoped task branches may read actor state under the sequential or
cooperative executor.

Concurrent mutation of actor state is rejected until Nulang defines an explicit
transaction/merge model.

Worker-parallel branches may not directly access actor-affine state.

Communication with actors remains available through actor messaging subject to
effect semantics.

## 10. Durable computation

Ordinary `par` is ephemeral scoped concurrency.

A durable workflow may use the same structured-task semantics internally, but
durability remains an orthogonal property supplied by workflow history,
replay-safe effects, timers/signals, and persistence.

The workflow subsystem must not reinterpret ordinary `par` as a durable
identity.

## 11. Diagnostics and DX

The compiler should explain why a region is legal but not worker-parallel.

Examples:

```text
par branch 1 captures mutable actor-affine value 'cache'
note: the region is valid and will run on the owning shard
help: use an immutable value or transfer unique ownership to enable worker parallelism
```

```text
par branch 0 performs Payments.charge
note: operation concurrency semantics are unknown
help: this region remains valid but cannot be worker-parallelized
```

Unsafe cross-branch dependencies remain compilation errors.

Performance eligibility should generally be a diagnostic/optimization property,
not a reason to reject otherwise well-defined structured concurrency.

## 12. Implementation sequence

### Phase A — semantic preservation

- land RFC 0024 execution-domain vocabulary;
- preserve `par` through HIR/MIR;
- reject lexical cross-branch mutation hazards;
- define operation semantics in one compiler module.

### Phase B — permanent source contract

- change `par` result typing/lowering to source-ordered tuples;
- add conformance coverage for zero/one/multiple branches;
- specify deterministic multi-failure selection;
- forbid parent-frame control-flow escape.

### Phase C — cooperative executor

- add explicit scoped-task group runtime state;
- implement source-order join;
- implement deterministic sibling cancellation;
- keep actor-affine captures on the owning shard;
- prove equivalence against the sequential reference mode.

### Phase D — ownership-aware worker execution

- reuse MIR ownership transfer proofs;
- classify Copy/FrozenShare/Move/ActorAffine captures;
- permit worker scheduling only for proven-safe groups;
- preserve ORCA thread confinement.

### Phase E — optimizer/runtime convergence

- replace ad-hoc markers with structured region metadata where appropriate;
- feed effect-operation semantics into workflows/replay and optimization legality;
- add deterministic scheduler/property tests;
- benchmark cooperative and worker modes independently.

## 13. Acceptance criteria

RFC 0025 is implementation-complete when:

- `par` returns typed source-ordered branch results;
- zero/one/multi-branch result shapes have conformance tests;
- child lifetime cannot escape the lexical scope;
- failure selection is deterministic across scheduler interleavings;
- sibling cancellation and cleanup are tested;
- cross-branch unsafe mutation is rejected statically;
- unknown effects fail closed for worker scheduling;
- actor-affine values never reach unsafe worker-thread heap access;
- unique ownership can be transferred using the common MIR ownership proof;
- sequential and concurrent hosts pass semantic-equivalence tests;
- ORCA's normal actor-local refcount path remains non-atomic/thread-confined;
- no task feature requires a new actor role.

## Compatibility

`par` is currently Experimental, so changing its result from "last expression
wins" to a typed source-ordered tuple is permitted before stabilization.

No frozen bytecode, persistence, or NUL0 wire format changes are introduced by
this RFC itself.

Any future serialized task-region metadata must receive its own explicit format
versioning decision.
