# RFC 0022: Atomic Durable Transitions

- **Status:** Draft
- **Tier:** Stable
- **Author:** David Porkka / implementation review
- **Created:** 2026-09-22
- **Resolved:** (pending)
- **Language-version at effect:** N/A (runtime/storage contract; no Frozen syntax change)
- **Supersedes:** none
- **Superseded by:** none

## Summary

Introduce one runtime-level atomic commit primitive for durable computation.
A durable actor/entity/workflow transition is committed as a single logical
record containing the state checkpoint or delta plus every Nulang-owned
observable consequence of that transition: message-journal metadata, workflow
events, domain events, timers, durable-effect state, compensation state, and
durable outbound messages.

The central invariant is:

> No Nulang-owned durable transition becomes observable unless all of its
> required durability records commit together.

External systems remain outside this atomic boundary. External effects use the
existing `DurableEffectId` Prepared/Completed state machine: commit Prepared,
dispatch using the stable operation id when the dependency supports
idempotency, then commit Completed.

This RFC is primarily a runtime and persistence contract. It adds no required
source-language syntax.

## Motivation

Nulang already has most of the pieces needed for durable computation:
persistent actors, event-sourced entities, workflow journals, durable timers,
sagas, virtual actors, deterministic replay infrastructure, and explicit
durable-effect identity.

The missing piece is a single commit boundary.

Today the persistence interface exposes independent operations such as:

```text
append_journal(...)
append_workflow_event(...)
append_event(...)
save_snapshot(...)
```

Runtime code therefore performs multi-record transitions as several writes.
Even when every individual write is fsync'd and every error is propagated, a
process can crash between them.

For example, a successful workflow step can conceptually require:

```text
state mutation
+ StepCompleted
+ timer changes
+ emitted domain events
+ outbound messages
```

If state is checkpointed before `StepCompleted`, a crash may preserve the
mutation without preserving completion. If `StepCompleted` is written first,
a crash may preserve completion without preserving the state produced by the
step. Reordering the writes only moves the crash window.

A second problem is that durable external effects need a stable local commit
boundary before and after external dispatch. The existing
`src/durable_effect.rs` correctly models Prepared versus Completed and
explicitly refuses to promise arbitrary exactly-once execution, but that model
cannot become the normal execution path until Nulang can atomically commit the
rest of an actor transition.

A third problem is backend semantic drift. Memory, JSON-file, libSQL,
PostgreSQL, and RocksDB can each make an individual write durable, but the
runtime currently has no common operation that means "all records in this
logical transition committed or none did."

## Design

### 1. Canonical runtime object

Add a versioned internal transition type in
`src/runtime/persistence.rs`:

```rust
#[derive(Debug, Clone)]
pub struct DurableTransition {
    pub version: u16,
    pub actor_id: u64,
    pub activation_epoch: u64,
    pub sequence: u64,
    pub expected_previous_sequence: u64,

    pub command: Option<JournalEntry>,
    pub snapshot: Option<ActorSnapshot>,

    pub workflow_events: Vec<WorkflowEvent>,
    pub domain_events: Vec<EventEntry>,

    pub durable_effects: Vec<DurableEffectPersistenceRecord>,
    pub outbox: Vec<DurableOutboxMessage>,
}
```

The initial persistence format version is 1.

`activation_epoch` is included now even before cluster-wide entity fencing is
fully implemented. Once virtual-actor ownership uses leases/epochs, a stale
activation must not be able to commit a transition after ownership moved.

`expected_previous_sequence` provides optimistic fencing even within one
logical actor history. A backend must reject a transition whose predecessor is
not the actor's current committed sequence.

### 2. Commit API

Extend `PersistenceStore` with:

```rust
fn commit_transition(
    &mut self,
    transition: DurableTransition,
) -> io::Result<DurableCommit>;
```

where:

```rust
pub struct DurableCommit {
    pub actor_id: u64,
    pub activation_epoch: u64,
    pub sequence: u64,
}
```

A successful return means every Nulang-owned record in the transition is
durable according to that backend's configured durability policy.

A failure means callers must assume none of the transition is committed.
Backends that cannot meet this contract must return
`io::ErrorKind::Unsupported`; they must not implement it as a sequence of
best-effort writes.

### 3. Runtime transaction lifecycle

For a durable actor activation:

```text
receive command
    |
begin in-memory transition
    |
execute deterministic/local code
    |
stage:
  state changes
  domain events
  workflow events
  timers
  durable outbox entries
  durable-effect records
    |
commit_transition()
    |
+-------------------+
| success           | failure
v                   v
publish effects     discard activation state
continue            recover last committed state
```

State may be mutated in memory while a handler runs for performance. It is not
considered committed state until `commit_transition` succeeds.

If commit fails, the runtime must not continue using partially mutated actor
state. The actor activation is discarded and reconstructed from the latest
committed transition/snapshot before more application messages are processed.

This fail-and-recover rule is intentionally simpler and safer than attempting
arbitrary in-place rollback of actor heap mutations.

### 4. What belongs inside the atomic boundary

The transition includes all state owned by Nulang whose visibility changes as a
result of one logical actor step:

- durable actor fields or a checkpoint/delta;
- the input command/message journal record;
- `WorkflowEvent::StepCompleted` / `StepFailed`;
- durable timer set/cancel/fire records;
- signal acceptance records;
- saga/compensation progress;
- entity/domain `emit` records;
- durable-effect Prepared/Completed records;
- durable outbound message/outbox entries;
- authority metadata changes when those become mutable.

The transition does **not** include:

- external HTTP/API/database commits outside the configured persistence store;
- arbitrary FFI effects;
- remote systems that do not participate in the Nulang store transaction;
- ephemeral/local actor state;
- tracing/logging telemetry.

Those are handled through explicit effect semantics rather than pretending they
share a distributed transaction.

### 5. External durable effects

The existing `DurableEffectRecord` remains the semantic model.

For an external effect performed by durable computation:

```text
transition N:
  DurableEffectRecord::Prepared(id, request_digest)
COMMIT

dispatch external request
  idempotency key = id when supported

transition N+1:
  DurableEffectRecord::Completed(id, result)
COMMIT
```

If Nulang crashes after Prepared but before external dispatch, recovery follows
the declared delivery semantics.

If it crashes after the external system commits but before Completed is
durable, the outcome is unknown. Recovery retries using the same
`DurableEffectId` when the dependency supports deduplication, retries
at-least-once when that is the declared contract, or delegates to the backend
for `BackendDefined`.

There is deliberately no generic ExactlyOnce variant.

### 6. Durable outbox

Messages emitted to other durable actors during a committed transition must be
staged in an outbox record in the same transition.

After commit, the runtime may deliver them at least once. Receivers deduplicate
using a stable message id derived from:

```text
(sender_actor_id, sender_epoch, transition_sequence, outbox_ordinal)
```

This prevents the classic:

```text
save state
CRASH
send message
```

and:

```text
send message
CRASH
save state
```

split-brain between actor state and messaging.

Ephemeral actor sends may retain their current lower-overhead semantics.

### 7. Timers

A durable timer is an outbox-like consequence of a transition.

A timer must not enter the live timer wheel until its containing transition
commits. A fired timer is likewise acknowledged as fired only through a
committed transition before the corresponding durable workflow transition is
allowed to advance.

Timer identity is stable across restart:

```text
(actor_id, activation_epoch, timer_name_or_id, set_sequence)
```

Re-arming after recovery is derived from committed timer state only.

### 8. Event-sourced entities

A source-level `emit` stages one domain event. The runtime never invents a
domain-state mutation.

`apply` or explicit behavior code determines state. The resulting state and
the emitted event(s) are committed together.

For multiple event-sourced fields affected by one event, persistence keys must
permit several `EventEntry` values at the same actor sequence. A two-column
`(actor_id, sequence)` key is insufficient; relational backends require at
least `(actor_id, sequence, field_name)` for the current representation.

Longer term, the preferred representation is one domain event plus state
projection/checkpoint metadata rather than duplicating the event once per
field. That normalization can happen independently because this RFC defines
atomicity, not the physical schema.

### 9. Workflow recovery

Workflow recovery consumes only committed transitions.

A committed transition is the source of truth for whether a workflow step
advanced. Recovery must never infer completion from a snapshot that is newer
than the completion record or vice versa because such split states cannot exist
after migration to this contract.

Old pre-transition persistence remains readable during the compatibility
period, but is marked legacy and retains its historical weaker crash semantics.

### 10. Activation epochs and fencing

Every durable commit carries an activation epoch.

Initially, single-node explicit-spawn actors may use epoch 1.

Virtual/distributed entities eventually obtain epochs from the ownership/lease
layer. Persistence rejects:

```text
transition.epoch < stored_epoch
```

and rejects a same-epoch transition whose
`expected_previous_sequence` does not match the committed tail.

This gives Nulang two independent protections:

1. **epoch fencing** against a stale activation after migration/failover;
2. **sequence fencing** against duplicated or reordered commits within an
   activation.

The 48-bit NaN-boxed actor handle is not the durable identity contract. It is a
runtime handle. A future identity RFC may widen/logically separate
`EntityId`; this transition format must be versioned so that change is
migratable.

### 11. Backend requirements

#### MemoryStore

Apply all components while holding the store's exclusive mutable borrow. Since
there is no I/O failure between component mutations, the implementation can
validate first and then apply.

#### libSQL / SQLite

Use one database transaction for:

- snapshot upsert;
- journal append;
- workflow/domain event inserts;
- effect records;
- outbox records;
- committed-tail update.

The transaction commits only after all statements succeed. Local SQLite
`synchronous=FULL` remains the strongest default durability mode.

#### PostgreSQL

Use one SQL transaction with a per-actor committed-tail row locked/updated by
compare-and-set semantics.

#### RocksDB

Use one `WriteBatch` spanning the relevant column families, followed by a
synchronous WAL write/flush according to the backend's durable-write policy.

#### JSON-file store

The current independent snapshot/log files cannot satisfy this RFC by a series
of renames.

Introduce an append-only per-actor transition journal as the canonical commit
record. Each record contains the complete logical transition plus checksum and
format version. Append + fsync of that single record is the commit point.
Snapshot/event files may remain derived indexes/caches, but recovery must be
able to rebuild solely from committed transition records.

A torn final record is ignored only if checksum/length framing proves that it
was never fully committed; corruption of an earlier committed record fails
closed.

### 12. Storage representation

Each backend stores a committed tail:

```rust
struct DurableTail {
    activation_epoch: u64,
    sequence: u64,
}
```

Transition sequence is monotonic per durable identity.

A commit is accepted iff:

```text
transition.activation_epoch > tail.activation_epoch
  and transition.expected_previous_sequence is valid for the epoch handoff

OR

transition.activation_epoch == tail.activation_epoch
  and transition.expected_previous_sequence == tail.sequence
  and transition.sequence == tail.sequence + 1
```

Duplicate submission of the exact already-committed transition may be treated
as idempotent success if its content digest matches. Same identity/sequence
with different content fails closed.

### 13. Transition digest

Each transition has a canonical BLAKE3 digest over its versioned serialized
content.

This supports:

- idempotent retry after an ambiguous local storage acknowledgement;
- corruption detection;
- deterministic testing;
- replication verification;
- future shadow-replica comparisons.

The digest is not a replacement for an activation epoch or sequence fencing.

### 14. Compiler/language impact

No new syntax is required.

The compiler already lowers entities, workflows, agents, and state machines
onto the actor substrate. This RFC deliberately keeps atomic durability in that
shared substrate.

Possible future syntax such as:

```nulang
durable { ... }
```

must lower to the same runtime transition mechanism rather than introducing a
second durable execution model.

### 15. Observability

Every committed transition should expose a causal identifier:

```text
actor logical id
activation epoch
transition sequence
transition digest
```

Tracing may attach this identifier to messages/effects so operators can follow
a transition across retries and restarts.

Telemetry is emitted only after commit and must not affect commit success.

### 16. Failure semantics

The runtime must make the following cases explicit:

| Failure point | Required behavior |
|---|---|
| Before transition commit | No durable progress |
| During backend commit | Treat as uncommitted/ambiguous; verify by digest/tail before retry |
| After commit, before caller sees success | Retry commit idempotently by sequence+digest |
| After effect Prepared commit, before dispatch | Recover according to delivery policy |
| After external commit, before Completed commit | Outcome unknown; retry using same durable effect id if permitted |
| After outbox commit, before delivery | Redeliver from outbox |
| After delivery, before outbox acknowledgement | Redeliver; receiver deduplicates |
| Commit rejected by stale epoch | Activation must stop; it has lost ownership |

### 17. Deterministic simulation testing

The DST harness should inject failure at every transition boundary:

1. before storage write;
2. after each staged component but before backend commit;
3. immediately after backend commit;
4. before/after external effect dispatch;
5. before/after outbox delivery;
6. during ownership epoch change.

For a fixed seed, recovery must converge to one of the explicitly permitted
states and never a partially committed Nulang-owned transition.

Property tests should assert:

```text
committed state == fold(committed transitions)
no stale epoch commits
no skipped sequence
no duplicated domain transition
no visible durable timer without committed TimerSet
no completed effect without Prepared predecessor
```

### 18. Implementation sequence

#### Phase A — storage contract

- Add `DurableTransition`, `DurableCommit`, digest, and tail validation.
- Add `PersistenceStore::commit_transition`.
- Implement MemoryStore first with exhaustive unit tests.
- Implement libSQL, PostgreSQL, RocksDB transactional commits.
- Implement the canonical JSON transition journal.

No runtime call site changes yet.

#### Phase B — workflow commit path

- Build one transition per workflow step.
- Commit state + `StepCompleted`/failure + timer/signal/saga state together.
- On commit failure, discard/recover the activation.
- Remove split `append_*(); checkpoint_actor()` paths.

#### Phase C — entity/domain events

- Stage `emit` records in the current transition.
- Commit domain events and resulting state together.
- Keep event-sourced state reconstruction compatible with existing journals.

#### Phase D — durable effects

- Route external `perform` calls from durable actors through
  `DurableEffectId`.
- Commit Prepared before dispatch.
- Commit Completed before returning the result to durable computation.
- Expose stable idempotency keys to HTTP/LLM/storage adapters that support
  them.

#### Phase E — durable outbox

- Stage actor sends in the transition.
- Redeliver unacknowledged committed outbox messages.
- Add receiver-side deduplication.

Implementation status (2026-09-24): transition-level outbox persistence is
live in the atomic storage contract, and PostgreSQL now exposes stable
RFC-identity pending/acknowledgement plus receiver-dedup persistence primitives.
The runtime dispatcher is deliberately still pending: inbox dedup acceptance
must be committed atomically with the receiver's command/state transition so a
crash cannot persist "seen" before the message has actually become durable
receiver progress.

#### Phase F — distributed ownership

- Connect virtual-actor activation ownership to epochs/leases.
- Reject stale commits at the persistence layer.
- Add DST partition/failover coverage.

## Tier Classification

**Stable runtime semantics.**

The source syntax need not change, but this RFC strengthens the meaning of
existing Stable durable constructs. Implementations must not claim the new
atomic transition guarantee until their configured persistence backend supports
`commit_transition`.

During rollout, capability/feature reporting should distinguish:

```text
durable_persistence
atomic_durable_transitions
durable_external_effects
durable_outbox
activation_fencing
```

so documentation cannot accidentally claim stronger guarantees than the
selected backend/runtime path provides.

## Backwards Compatibility

Existing source programs remain valid.

Existing persisted data remains readable through legacy snapshot/journal/event
readers. Once an actor makes its first atomic transition, the backend records a
transition-format marker and all subsequent commits use the new path.

Historical records written before this RFC retain their old crash-consistency
limits. Migration must not fabricate atomicity for history that was originally
written as separate operations.

Backends may compact legacy history into a new snapshot only after recovery has
successfully reconstructed the actor and that snapshot is committed as the
first atomic transition.

## Alternatives Considered

### 1. Propagate every existing persistence error

Necessary but insufficient. It prevents some false-success paths but does not
remove crash windows between separately successful writes.

### 2. Choose a better ordering for snapshot and event writes

Rejected. Any ordering has a crash point that exposes one without the other
unless recovery can reconstruct the missing half. Durable external effects and
arbitrary state mutation make such reconstruction unsafe in the general case.

### 3. Make workflows pure deterministic replay only

Rejected as the universal model. Deterministic replay is useful, but Nulang
also supports actors, event-sourced entities, mutable local execution,
capabilities, and effects. The runtime still needs an explicit durable commit
boundary.

### 4. Require an external distributed transaction manager

Rejected. Most external APIs cannot join one, and Nulang's own state can be
made atomic without imposing two-phase commit on every dependency.

### 5. Claim exactly-once execution

Rejected. Nulang can guarantee exactly-once application of its own committed
transition and effectively-once external behavior where a dependency supports
deduplication. It cannot generally prove exactly-once effects across an
arbitrary network boundary.

## Open Questions

1. Should the first implementation store full snapshots in every transition or
   use deltas plus periodic snapshots? This is a performance choice, not a
   semantic one.
2. Should durable outbox acknowledgement state share the actor transition log
   or use a separate compact delivery index?
3. What logical identity width replaces the current 48-bit runtime actor handle
   for long-lived distributed entities?
4. Should backend capability negotiation be exposed through a language/runtime
   introspection effect or only deployment metadata?
5. When a commit fails, should the runtime synchronously reconstruct the actor
   immediately or mark the activation unavailable and lazily recover on the
   next message? Either is valid if uncommitted state is never observable.

## Resolution

(To be filled on accept/reject.)
