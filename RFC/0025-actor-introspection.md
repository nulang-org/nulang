# RFC 0025: First-Class Actor Introspection

- **Status:** Draft — read model implemented
- **Created:** 2026-09-20
- **Depends on:** RFC 0017 unified runtime primitives

## Summary

Nulang should make actor state operationally inspectable without exposing mutable runtime internals. A production operator needs to answer questions such as:

- Where is this actor in its lifecycle?
- Is its mailbox backing up?
- Is it persistent, suspended, or hibernated?
- What is its journal/checkpoint position?
- Who supervises, monitors, or links to it?
- How much work has it processed?
- Does it hold object-store references or runtime authority grants?

This RFC introduces a read-only, serializable inspection model over state the runtime already tracks.

## Phase 1 API

```text
inspect_actor(runtime, actor_id) -> ActorInspection?
inspect_actors(runtime) -> ActorFleetInspection
```

The API does not mutate actor state, scheduler queues, mailboxes, supervision trees, or persistence.

## Actor inspection fields

Phase 1 includes:

- actor id and name;
- lifecycle state;
- actor kind: agent, workflow, persistent actor, transient actor;
- execution backend;
- scheduling priority;
- mailbox depth/capacity/utilization;
- behavior and state-field counts;
- dirty-field count;
- object-store references held;
- query-handler count;
- lifetime reductions and per-turn reduction budget;
- durable sequence/checkpoint position;
- event-log and event-sourced-field counts;
- persistence/suspension/waiting-signal state;
- hibernation, idle age, pinning;
- parent/children/monitors/links/trap-exit configuration;
- runtime authority grant count;
- flight-recorder depth.

Relationship lists are sorted in the read model so repeated snapshots are deterministic even if internal insertion order changes.

## Sensitive data

Raw runtime-authority tokens may include host names, file paths, bucket names, model identifiers, tenant resources, or other sensitive operational context.

The default inspection model therefore exposes only `capability_count`.

A future privileged diagnostics endpoint may return token details only after authorization and redaction policy are applied.

The same principle should apply to message payloads, actor state values, LLM prompts, and external-effect arguments.

## Mailbox pressure

For bounded mailboxes:

```text
utilization = depth / capacity
```

For unbounded mailboxes, utilization is `None` rather than an invented percentage.

Future metrics should also expose:

- oldest-message age;
- enqueue/dequeue rate;
- rejected messages/backpressure count;
- system/application message breakdown;
- selective-receive scan cost.

Those require additional counters and are not fabricated from the current runtime state.

## Identity and placement follow-up

Phase 1 observes current activation actor ids. Once logical EntityId / ActivationId separation is fully integrated, introspection should expose both:

```text
logical_entity_id
activation_id
node_id
region
placement_reason
activation_generation
```

Operational tooling must never confuse permanent logical identity with an ephemeral activation handle.

## OpenTelemetry integration

The inspection read model is suitable as the common source for:

- CLI actor inspection;
- DAP actor panes;
- HTTP/admin endpoints;
- Prometheus/OpenTelemetry metrics;
- structured logs;
- crash reports;
- support bundles.

Exporters should consume the read model rather than each reaching into runtime internals independently.

Recommended metric families include:

```text
nulang.actor.mailbox.depth
nulang.actor.mailbox.utilization
nulang.actor.reductions
nulang.actor.durable.sequence
nulang.actor.idle_ms
nulang.actor.held_objects
nulang.actor.flight_recorder.entries
```

High-cardinality actor ids should not automatically become metric labels. Per-actor detail belongs in traces/logs/admin inspection; aggregate metrics should normally group by actor type/role, tenant, node, or placement class.

## Consistency semantics

An inspection is a point-in-time read of one runtime's current state. It is not a distributed transaction across the cluster.

Cluster-wide inspection should attach:

- node id;
- observation timestamp/monotonic generation;
- directory epoch where applicable;

and explicitly tolerate actors moving between observations.

## Failure semantics

Inspecting a missing activation returns no result. It must not implicitly activate a virtual actor merely to inspect it.

A future logical-entity inspection API may separately report directory/dehydrated state without hydration.

## Decision

Actor observability becomes a first-class read model over runtime state. Mutating debugging operations such as state replacement, forced restart, or mailbox manipulation remain separate privileged APIs and must not be mixed into the inspection surface.
