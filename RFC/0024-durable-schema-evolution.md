# RFC 0024: Durable Event and Snapshot Schema Evolution

- **Status:** Draft — core primitives implemented
- **Created:** 2026-09-16
- **Depends on:** RFC 0017 unified runtime primitives

## Summary

Durable actors must be able to replay data written by older code. Event sourcing is not production-safe if event and snapshot payloads have no explicit schema version or deterministic migration path.

This RFC introduces a storage-backend-neutral evolution layer with:

- explicit `SchemaVersion` values;
- `VersionedEvent` envelopes;
- `VersionedSnapshot` envelopes;
- contiguous deterministic `vN -> vN+1` migration chains;
- fail-closed handling for future schemas and missing migrations;
- projection cursors that tolerate replayed duplicates but reject journal gaps.

## Current gap

The current persistence structs record event sequence/name/arguments/value and actor snapshot sequence/state, but neither durable envelope carries an application schema version. This means a field or event representation can become unreadable when code changes while old journal entries remain.

## Design principles

### Version persisted data, not implementation functions

A logical event type has a stable name independent of the function currently applying it.

```text
UserCreated v1
UserCreated v2
UserCreated v3
```

Renaming source functions must not silently rename persisted event identity.

### One migration step per version

Migrations advance exactly one version:

```text
v1 -> v2 -> v3 -> v4
```

The runtime does not search a graph for an arbitrary migration route. Contiguous chains make missing production migrations explicit and deterministic.

### Migrations are pure

Upcasters must be deterministic and side-effect free. Recovery, replicas, projections, debugging, and disaster-recovery tooling may execute them repeatedly.

A migration must not depend on:

- wall clock time;
- network services;
- random values;
- mutable external databases;
- the current actor activation location.

### Future data fails closed

If a runtime targeting schema v3 reads v4 data, it must reject it explicitly. Running an older binary must not guess how to interpret future durable state.

## Event envelope

```text
VersionedEvent {
  event_type
  schema_version
  sequence
  payload
}
```

The first implementation uses `serde_json::Value` as the migration document because existing persistence backends already depend on serde/JSON and migrations should operate on a stable serialization boundary rather than VM heap pointers.

This does not require JSON to become Nulang's permanent event wire format. A future binary format can preserve the same logical versioning contract.

## Snapshot envelope

```text
VersionedSnapshot {
  schema
  schema_version
  sequence
  state
}
```

Snapshots use the same migration registry semantics as events. This prevents a common failure mode where journals are versioned but old snapshots become unreadable and force full-history replay.

## Migration registry

Each schema declares a current write target and zero or more one-step migrations.

```text
set_target(UserCreated, v3)
register(UserCreated, v1 -> v2)
register(UserCreated, v2 -> v3)
```

Before deployment/recovery, `validate_chain` can verify that every supported historic version reaches the current target.

Duplicate migration definitions are rejected.

## Projection rebuild contract

Projection cursors track the last applied journal sequence.

Rules:

- `sequence <= last_sequence`: already applied, safe duplicate/replay;
- `sequence == last_sequence + 1`: apply and advance;
- `sequence > last_sequence + 1`: fail with an explicit gap.

A projection must never silently skip missing events.

## Storage migration plan

The initial primitive is additive and does not yet mutate `EventEntry` or `ActorSnapshot` persisted layouts.

Follow-up migration should be staged:

1. Add schema/version fields with serde-compatible defaults for legacy records.
2. Interpret legacy entries as `SchemaVersion::LEGACY` (`v0`).
3. Require an explicit `v0 -> v1` migration for types whose legacy representation needs transformation.
4. Write all new events/snapshots at the declared target version.
5. Upcast on recovery before applying event logic.
6. Upcast snapshots before restoring actor state.
7. Add offline journal/snapshot rewrite tooling only as an optimization; replay correctness must not depend on eagerly rewriting old data.

## Rolling upgrades

Schema compatibility should enable heterogeneous cluster versions rather than arbitrary in-place hot code replacement.

A rolling deployment may run code v3 and v4 concurrently only when their declared durable schema read/write compatibility permits it. A node that cannot read the current journal target must not activate that entity.

Future actor protocol versioning can use the same compatibility metadata during placement and activation routing.

## Projection generations

A future extension should version projection definitions separately from event schemas. When projection logic changes semantically, operators can create a new projection generation and rebuild it from sequence zero or a compatible checkpoint without mutating the source event log.

## Operational requirements

Production introspection should expose:

- actor/entity durable schema target;
- oldest supported version;
- snapshot schema version;
- journal event versions encountered during recovery;
- migration steps executed;
- migration failures;
- projection cursor and detected gaps.

## Non-goals

This RFC does not:

- claim arbitrary old schemas can be migrated automatically;
- allow non-deterministic migrations;
- delete old events after migration;
- require one persistence backend;
- change event sourcing into generic exactly-once processing.

## Decision

Nulang durable state will use explicit versioned event/snapshot envelopes and deterministic contiguous migration chains. Schema compatibility becomes part of the actor activation contract and is a prerequisite for safe rolling upgrades of long-lived durable entities.
