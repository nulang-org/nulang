# RFC 0025: Temporal Compatibility Boundary

- **Status:** Draft
- **Tier:** Experimental compatibility surface
- **Created:** 2026-09-23
- **Supersedes:** none
- **Superseded by:** none

## Summary

Define Temporal compatibility as an adapter around Nulang's durable execution
model rather than as a second runtime architecture.

Nulang's canonical model remains actors/entities, atomic durable transitions,
timers, signals, durable effects, and stable identity. Temporal concepts are
translated at the boundary:

```text
Temporal SDK / WorkflowService
            |
            v
    Temporal compatibility
            |
            v
Nulang durable execution primitives
```

The compatibility layer may eventually allow existing Temporal SDK
applications to point at a Nulang endpoint, but Temporal-specific protobufs,
history event classes, retry objects, and server implementation details must
not become Nulang language or runtime primitives.

## Motivation

Temporal is a useful migration target because its worker/server boundary is
explicit: workers poll workflow or activity tasks, replay workflow history, and
respond with commands or activity results.

Nulang already has the lower-level mechanisms needed to represent the durable
semantics:

- atomic `DurableTransition` commits with sequence and activation fencing;
- replay-stable `DurableEffectId` values;
- Prepared/Completed durable-effect records;
- durable timers and signals;
- event-sourced/durable entities;
- outbox-ready transition structure;
- deterministic scheduler/testing infrastructure.

Reimplementing Temporal internally would duplicate these mechanisms and make
Nulang inherit Temporal's SDK-first programming model. The compatibility layer
should instead translate Temporal requests into Nulang-native durability.

## Non-goals

This RFC does **not**:

- add a `process` keyword;
- make `workflow` a permanent Nulang language primitive;
- require deterministic history replay as Nulang's only recovery model;
- claim exactly-once external activity execution;
- require full Temporal API coverage before the adapter is useful;
- require Temporal protobuf types in `src/runtime`.

## Architectural rules

### 1. One-way dependency

Protocol-specific code lives under `compat` or a future standalone gateway
crate/service.

Allowed:

```text
compat::temporal -> durable_effect
compat::temporal -> runtime::persistence public durable types
gateway -> compat::temporal
```

Forbidden:

```text
runtime -> compat::temporal
persistence backend -> Temporal protobuf
compiler/typechecker -> Temporal API
language syntax -> Temporal command/event types
```

### 2. Concrete workflow execution identity

A compatibility adapter must resolve a concrete Temporal execution:

```text
(namespace, workflow_id, run_id)
```

before creating replay-stable Nulang operation identities. "Latest run" is an
API lookup concept, not a durable identity.

### 3. Activity mapping

A Temporal activity command maps to an external durable effect.

Default contract:

```text
Temporal ScheduleActivity
    -> DurableEffectRecord::Prepared
    -> atomic transition commit
    -> activity dispatch
    -> DurableEffectRecord::Completed
    -> atomic transition commit
```

The default delivery semantic is `AtLeastOnce`.

`EffectivelyOnceWithDeduplication` is permitted only when the activity
adapter or downstream dependency has a real idempotency/deduplication
contract. Nulang must not silently strengthen Temporal activity semantics.

### 4. Stable operation identity

The compatibility adapter derives a `DurableEffectId` from replay-stable
inputs including:

- Nulang durable actor/entity identity;
- Temporal namespace/workflow/run;
- workflow-task started event identity;
- activity id;
- command ordinal;
- activity type.

Replaying the same workflow task and command therefore produces the same
Nulang durable-effect id.

### 5. Timers and signals

Temporal timer and signal history map to Nulang's existing durable timer/signal
records and are staged inside the same `DurableTransition` as state progress.

The first implementation may use the existing `WorkflowEvent` representation.
That representation is an internal compatibility structure, not justification
for keeping `workflow` as a permanent language keyword.

### 6. History is input, not Nulang's ontology

A Temporal gateway may need to expose Temporal history because existing SDKs
expect it. The gateway may materialize that history from committed Nulang
transitions.

Internally, Nulang remains free to recover from snapshots, transition journals,
event-sourced state, or other future mechanisms as long as the adapter can
produce behavior compatible with the supported Temporal surface.

## Adapter contract versioning

Managed adapters consume a small semantic compatibility contract rather than
depending on Nulang compiler/runtime internals. The contract exposes
`TEMPORAL_COMPATIBILITY_CONTRACT_VERSION`, initially version 1.

Cloud or another managed gateway must compare its expected contract version
with the runtime-provided version before accepting Temporal work. An
incompatible change to adapter identity, replay, command planning, or durable
effect semantics increments this version independently from Nulang artifact
formats and independently from Temporal's upstream API version.

## Compatibility phases

### Phase 0 — semantic adapter

Implement and test a protocol-independent Rust adapter boundary:

- concrete execution identity;
- stable activity effect identity;
- at-least-once activity preparation;
- optional deduplicated activity preparation;
- timer mapping;
- signal mapping.

No network endpoint is claimed in this phase. The worker core is deliberately
transport-neutral so the same replay/command semantics can be used by a
standalone Temporal worker transport and by Nulang Cloud's managed gateway.

### Phase 1 — Nulang worker for Temporal Server

Implement a standalone integration that can:

- poll Temporal workflow tasks;
- replay supported history into the Nulang execution adapter;
- return commands;
- poll and complete activity tasks.

This provides a low-risk adoption path because users keep their Temporal
server.

### Phase 2 — Temporal WorkflowService gateway over Nulang

Implement the minimum server-side surface required by common SDK applications:

- `StartWorkflowExecution`;
- `GetWorkflowExecutionHistory`;
- `PollWorkflowTaskQueue`;
- `RespondWorkflowTaskCompleted`;
- `RespondWorkflowTaskFailed`;
- `PollActivityTaskQueue`;
- activity completion/failure;
- workflow signals;
- cancellation/termination;
- timers;
- retries;
- queries.

The gateway translates these calls into Nulang durable identities,
transitions, effects, timers, and messages.

### Phase 3 — migration and broader parity

Add, based on real adoption demand:

- child workflows;
- continue-as-new;
- workflow updates;
- schedules;
- search attributes/visibility;
- reset;
- worker versioning/deployments;
- Nexus;
- namespace/admin surfaces.

Full Temporal administrative parity is not a prerequisite for positioning
Nulang as a migration target.

## Conformance strategy

Create a black-box compatibility suite that can run the same fixture against a
reference Temporal deployment and Nulang's compatibility endpoint.

Initial fixtures:

1. start -> activity -> completion;
2. activity retry after worker loss;
3. timer fire after restart;
4. signal while workflow is suspended;
5. workflow task replay produces identical commands;
6. duplicate activity dispatch preserves stable operation id;
7. crash after activity Prepared but before dispatch;
8. crash after external activity commit but before Completed;
9. query does not mutate durable state;
10. cancellation is durably observed.

The suite should compare externally observable behavior, not internal storage
layout.

## Implementation in this RFC

The initial implementation lives in:

```text
src/compat/mod.rs
src/compat/temporal.rs
```

It intentionally has no protobuf, gRPC, tonic, or Temporal SDK dependency.

The first adapter provides:

- `TemporalWorkflowExecution`;
- `TemporalWorkflowTaskContext`;
- `TemporalActivityRequest`;
- `TemporalActivityPlan`;
- stable length-prefixed execution keys;
- durable activity preparation;
- timer/signal mapping;
- a protocol-neutral supported-history projection;
- fail-closed history reference/order validation;
- a `TemporalWorkerCore` that prepares workflow tasks and plans ordered commands;
- replay/collision/history-validation tests.

## Future crate boundary

Once wire-level compatibility begins, move transport/protobuf concerns to a
separate crate or Nulang Cloud service, for example:

```text
crates/nulang-temporal-protocol
crates/nulang-temporal-worker
services/nulang-temporal-gateway
```

The core `compat::temporal` module should remain small and dependency-light or
be replaced by a similarly narrow shared semantic crate.

## Success criteria

Temporal compatibility is successful when all of the following are true:

1. Nulang-native programs do not need Temporal concepts.
2. Existing Temporal users can adopt Nulang incrementally.
3. Activity replay uses Nulang's durable-effect guarantees without false
   exactly-once claims.
4. Temporal wire/API churn can be absorbed in the adapter without changing the
   language or persistence model.
5. Nulang can eventually outperform or simplify Temporal-native deployments
   without sacrificing compatibility at the supported boundary.

## Alternatives considered

### Implement Temporal semantics directly in the runtime

Rejected. It creates a second durable execution architecture and couples
Nulang's core to another project's evolving protocol.

### Add a new `process` keyword and map Temporal workflows to it

Rejected for this phase. Nulang already has an orthogonal execution model and
a `Process` host-effect namespace. Adding another surface abstraction would
increase conceptual overlap without solving protocol compatibility.

### Require exact Temporal history replay internally

Rejected. Existing SDK compatibility may require externally compatible
history, but Nulang's storage/recovery architecture should remain free to use
its stronger atomic transition model.

### Wait until every Temporal API is implemented

Rejected. Worker compatibility and the core workflow/activity path provide
useful incremental adoption long before full administrative parity.

## Resolution

(To be filled on accept/reject.)
