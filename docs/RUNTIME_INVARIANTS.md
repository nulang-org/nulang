# Nulang Runtime Invariants

Status: implementation hardening contract

This document defines the runtime properties that must remain true across
compiler changes, runtime changes, process crashes, machine crashes, rolling
upgrades, network partitions, actor migration, and persistence backend changes.

These invariants take precedence over adding new runtime features. A feature
that cannot preserve them must remain experimental or be redesigned.

## 1. Actor identity

### 1.1 Logical identity is not activation identity

A durable/virtual actor's logical identity is stable across process lifetime,
node placement, rehydration, and migration. A physical activation is temporary.
The runtime must never use a physical activation address as the sole durable
identity of an entity.

For grains, `GrainId { grain_type, key }` is the authoritative logical
identity. `grain_actor_id()` is a compact 48-bit v1 projection used by the
current actor-ref representation, persistence keys, and shard routing.

### 1.2 The v1 projection is persistence ABI

The exact v1 `grain_actor_id()` mapping is frozen. It may only change by
introducing a new version plus an explicit persisted-state migration. Golden
vectors in `src/runtime/grain.rs` enforce this contract.

### 1.3 Compact-id collisions fail closed

A 48-bit projection is not collision-free. Whenever a runtime establishes a
mapping from compact actor id to `GrainId`, an existing different `GrainId`
for the same compact id must be treated as an identity collision. The runtime
must not silently alias the actors.

Long term, durable storage and wire protocols should carry a versioned logical
identity (or a collision-resistant digest of it) independently of the compact
in-process actor-ref payload.

## 2. Durable acknowledgement

A durable operation must not be reported as committed until the state needed to
recover that operation is on stable storage according to the selected
persistence policy.

For file-backed persistence this means successful append/write plus the
configured synchronization boundary. For transactional backends it means a
successful commit at the promised durability level.

No execution path may acknowledge a durable message and persist it only later
without an explicit weaker-durability contract visible to the caller.

## 3. Journal integrity

Recovery must replay a valid prefix of each journal. It must never skip a
malformed record and continue applying later records, because doing so creates a
state that never existed in the acknowledged execution history.

Required properties:

- entries are replayed in strictly increasing sequence order;
- duplicate sequence numbers are either idempotently identical or rejected;
- a gap is rejected only for a stream whose sequence contract is contiguous;
- a malformed interior record or invalid checksum fails recovery closed;
- a torn terminal append may be discarded only as an invalid suffix;
- `latest_sequence()` reports the last valid recoverable sequence, not merely
  the last parseable record after corruption;
- snapshot sequence and replay start sequence must agree.

Nulang has multiple persistence streams that may share/interleave actor-level
sequence numbers. A stream therefore must explicitly declare whether it is
merely strictly increasing or truly contiguous. `src/persistence_integrity.rs`
encodes and tests both policies so backend readers do not invent incompatible
sequence semantics.

## 4. Snapshot integrity

A snapshot replacement is atomic from the recovery reader's point of view.
Recovery sees either the previous complete snapshot or the new complete
snapshot, never a partially written representation.

Snapshots must eventually carry enough metadata to validate:

- format version;
- actor/logical identity;
- schema/behavior version;
- snapshot sequence;
- payload integrity (checksum or backend equivalent).

If the metadata is incompatible, recovery must return an explicit error rather
than silently resetting durable state.

## 5. Schema and behavior evolution

Durable state makes code upgrades part of language semantics.

For every persisted schema/behavior version accepted by a release, the runtime
must have exactly one of:

1. a deterministic migration chain to the current version;
2. an explicitly supported old-code execution path; or
3. a fail-closed incompatibility error before actor activation.

Migration chains are append-only once released. A released migration may not be
silently edited because that makes replay dependent on which runtime version
performed the migration.

## 6. Message dispatch

Behavior dispatch must fail closed.

An unknown behavior id/name, incompatible protocol version, arity mismatch, or
payload decode failure must never fall through to a different handler. The
runtime must surface a typed dispatch failure (and, where configured, place the
message in the dead-letter path) without mutating durable actor state.

## 7. Replay determinism

Given the same:

- persisted snapshot;
- ordered journal/event suffix;
- behavior/schema version;
- deterministic inputs recorded by the effect boundary;

the runtime must reconstruct the same durable state regardless of machine,
process, scheduler interleaving, or backend.

Wall clock, randomness, network responses, LLM responses, environment values,
and other nondeterministic inputs used by replayable code must pass through a
record/replay-capable effect boundary or be rejected from deterministic replay.

## 8. Distribution and migration

At any logical instant, a durable actor may have at most one writer for state
that is not explicitly CRDT/mergeable.

Node migration and node-loss recovery therefore require an activation epoch or
equivalent fencing token. A stale activation must be unable to commit after a
newer activation has become authoritative.

Routing caches and forwarding entries are hints, not identity. They may change
without changing logical actor identity.

## 9. Backpressure and scheduler fairness

The runtime must have bounded or policy-controlled growth for:

- actor mailboxes;
- pending remote messages;
- retry queues;
- pending bytecode fetches;
- timers;
- dead letters;
- per-tenant/per-node runnable work.

A single actor or tenant must not indefinitely starve unrelated runnable actors.
Fairness and overload behavior must be measurable with tail-latency metrics, not
inferred from average throughput.

## 10. Backend semantic equivalence

Interpreter, JIT, AOT/native, and WASM execution must agree on observable
language semantics for the supported common subset.

Differential tests should compare results, errors, effect ordering, and durable
state transitions. Backend-specific optimization is permitted; backend-specific
language meaning is not.

## 11. Destructive recovery test matrix

Production-readiness gates should include automated scenarios that repeatedly:

1. start a durable actor and commit state;
2. terminate the process at randomized persistence/replay points;
3. restart from the same store;
4. verify acknowledged-state preservation and monotonic sequence recovery.

The matrix should cover snapshots, message journals, event-sourced state,
workflow events, timers/signals, CRDT snapshots, rolling schema upgrades,
migration, duplicate delivery, torn terminal records, interior corruption, and
network partitions.

## 12. Release gates

Do not treat the following as marketing targets; treat them as reproducible
release tests with recorded hardware/software configuration:

- large idle-actor density without unbounded per-actor overhead;
- sustained active-actor load with bounded p99/p999 mailbox latency;
- repeated abrupt process termination with zero acknowledged-state loss;
- rolling upgrades across every supported persisted schema version;
- duplicate delivery without duplicated durable effects where idempotency is
  promised;
- network partition plus heal without two unfenced durable writers;
- actor migration while traffic continues;
- cross-backend differential conformance.

## Immediate implementation order

1. Freeze and version the existing grain-id projection. **Completed in the phase-1 hardening PR.**
2. Reject compact grain-id collisions at every binding point. **Typed fail-closed binding primitive implemented; runtime call-site wiring remains (#289).**
3. Make JSONL recovery consume a valid prefix and stop on corruption instead of
   skipping malformed records. **Strict parser and torn-tail policy implemented; backend wiring remains (#290).**
4. Validate journal sequence monotonicity/gaps during recovery. **Ordering policies and tests implemented; backend wiring remains (#290).**
5. Add persisted schema/behavior identity to snapshots and migration checks.
6. Add activation-epoch fencing for durable writer ownership.
7. Add subprocess kill/restart durability tests to CI.
8. Add differential interpreter/JIT/AOT/WASM conformance cases for durable
   semantics.
9. Add actor-density, mailbox-tail-latency, and scheduler-fairness benchmark
   gates.

Until these are green, new runtime subsystems should be considered lower
priority than invariant hardening.
