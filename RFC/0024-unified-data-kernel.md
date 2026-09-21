# RFC 0024: Unified Data Kernel

- **Status:** Draft — Phase 1 storage-native scans in progress
- **Tier:** Experimental
- **Created:** 2026-09-21

## Summary

Nulang should make durable application state, typed queries, derived views, and
change streams runtime concepts rather than requiring an external database
boundary for ordinary application state.

The language/runtime owns the **logical data model and semantics**. It does not
require one physical storage representation. Operational state, analytical
history, search indexes, vectors, and cold objects may use specialized engines
or layouts behind the same language contract.

The objective is stack collapse without turning the Nulang runtime into a
from-scratch replacement for every database engine.

## Principles

1. **Durability is a property of Nulang state, not a save operation.**
2. **One committed change model feeds recovery, replication, indexes, views,
   analytics, search, subscriptions, and external CDC.**
3. **Logical unification does not imply physical uniformity.** Point reads,
   analytical scans, full-text search, and object blobs have different optimal
   representations.
4. **The compiler and runtime share the data model.** Index and materialization
   diagnostics should be possible before production.
5. **Local development and distributed production use the same source model.**
   Deployment topology changes; application persistence code should not.
6. **Existing engines are adapters when they are already excellent at a
   physical problem.** Nulang should not rebuild PostgreSQL, ClickHouse,
   Tantivy, Lance, or object storage merely to claim stack ownership.

## Logical model

The target user-facing data model has five concepts:

### Entity

Identity-bearing mutable durable state with actor ownership and a single
serialization point.

```nulang
durable entity Account(id: AccountId) {
    balance: Money
    status: AccountStatus
}
```

Entity mutations are durable runtime commits. There is no mandatory ORM,
repository, or explicit `save()`.

### Relation

High-volume typed records optimized for bulk append, relational access, and
analytical projection rather than actor lifecycle.

```nulang
relation Impression {
    campaign: CampaignId
    site: SiteId
    at: Instant
    views: Int
    spend: Money
}
```

A relation may use row-oriented or columnar physical storage depending on the
workload and declared policy.

### Stream

An ordered sequence of committed typed changes. Streams are the integration
surface for replication, subscriptions, external CDC, and asynchronous
processing.

The runtime-owned durable change feed introduced in Phase 0 is the first
implementation of this concept.

### View

A typed derived query. A normal view may be computed on demand.

```nulang
view ActiveCustomers =
    Customer.where(.status == .active)
```

### Materialized view

A derived value maintained incrementally from committed changes.

```nulang
materialized view MonthlyRevenue =
    Invoice
        .where(.status == .paid)
        .group_by(.paid_month)
        .sum(.amount)
```

Materialized views should update from deltas whenever the operator graph
supports incremental maintenance; full recomputation is a fallback, not the
default architecture.

## Commit and change semantics

The current persistence layer already has three durable append-only sources:

- delivered-message journal;
- event-sourced state entries;
- workflow event journal.

Phase 0 exposes these through one deterministic per-actor API using
`DurableChange`.

A cursor is the tuple:

```text
(sequence, lane, ordinal)
```

A sequence number alone is not sufficient because multiple committed records
may share one actor sequence.

The Phase 0 adapter establishes consumer semantics without changing storage.
It is not the final high-throughput implementation because existing
`PersistenceStore` read methods materialize complete source logs.

### Storage-native follow-up

The persistence contract should gain a storage-native range scan / commit-log
API with:

- bounded reads;
- exclusive resume cursor;
- no full-log materialization;
- monotonic commit identity that survives compaction;
- atomic publication with the underlying durable state transition;
- retention/compaction metadata.

Consumers must depend on the logical `DurableChange` contract rather than a
specific libSQL/JSON/in-memory representation so this optimization does not
change application semantics.

## Query model

Nulang query syntax should compile to a typed logical plan before selecting a
physical backend.

A query planner may choose among:

- direct entity lookup;
- secondary index;
- local collection scan;
- embedded SQLite/libSQL;
- incremental materialized view;
- columnar analytical projection;
- remote/distributed execution.

SQL remains an interoperability and physical-adapter surface, not the canonical
language semantics.

This supersedes the assumption that every query comprehension should ultimately
translate to SQL.

## Indexes

Indexes should be language declarations associated with the compiler-owned type
model:

```nulang
unique index Customer.email

index Customer.by_company_status {
    company
    status
}
```

The compiler/runtime may then diagnose unindexed hot paths and recommend an
index or materialized view using observed runtime cardinality/traffic data.

Index maintenance consumes the same committed durable change feed as every
other projection. Application code must not own cache/index invalidation.

## Transaction boundaries

Nulang should exploit actor/entity ownership rather than provide global
distributed transactions by default.

- One entity: serializable through its actor serialization point.
- Colocated entity group: optional local ACID transaction.
- Independent partitions: durable workflow/saga with compensation.
- Global serializability: explicit exceptional capability, not the default.

This keeps coordination costs proportional to the operation rather than every
write.

## Operational and analytical physical representations

The same logical data may have several physical projections:

```text
runtime commit
     |
     +--> hot entity state / secondary indexes
     |
     +--> incremental materialized views
     |
     +--> immutable columnar analytical segments
     |
     +--> full-text/vector indexes
     |
     +--> external CDC
```

Operational state should optimize point/range access and mutation.

Analytical projections should eventually support:

- immutable columnar segments;
- column pruning;
- partition pruning;
- zone maps;
- dictionary/compression codecs;
- vectorized/SIMD execution;
- parallel scans;
- spill to disk;
- cold segments in object storage.

Nulang does not need to implement every primitive from scratch. An embedded
engine may provide the physical implementation if the language contract and
change semantics remain Nulang-owned.

## Nulang Cloud mapping

Nulang Cloud should treat runtime durable changes as the source of truth.

`nlc-events` adds trusted tenant identity at the host boundary and partitions
changes by tenant+actor to preserve actor ordering. It may then fan changes out
to:

- analytical projection workers;
- search/vector indexers;
- subscriptions;
- replication;
- audit pipelines;
- external sinks.

Cloud adapters retain specialized responsibilities:

- `nlc-db`: embedded SQL/libSQL compatibility and relational physical access;
- `nlc-storage`: object/cold storage;
- `nlc-search`: full-text physical index;
- `nlc-vector`: vector physical index;
- `nlc-events`: transport/fan-out, not the authoritative state model.

This allows those components to be replaced or optimized without changing the
language's data semantics.

## Local-to-cloud requirement

A program using entities, relations, views, materialized views, and streams
must be able to run locally with no external service.

The same source may scale to Nulang Cloud where the runtime introduces
partitioning, replication, tiered storage, analytical workers, and object-store
offload.

No application rewrite from SQLite -> PostgreSQL -> Redis -> Kafka -> warehouse
should be required solely because deployment scale increases.

## Non-goals

This RFC does not propose:

- a from-scratch PostgreSQL-compatible database engine;
- a from-scratch Kafka-compatible broker;
- one storage layout for OLTP and OLAP;
- transparent global serializable transactions;
- automatic materialization without cost/consistency observability;
- replacing object storage for large immutable blobs.

## Implementation phases

### Phase 0 — unified durable change contract

- [x] Runtime `DurableChange` abstraction over journal/event/workflow records.
- [x] Composite lossless resume cursor.
- [x] Stable JSON lane/record names.
- [x] Cloud tenant envelope and actor-stable event partitioning.
- [ ] CI and cross-repository contract validation.

### Phase 1 — storage-native change scans

- [x] Add bounded `scan_durable_changes(after, limit)` with an exclusive
  composite cursor.
- [x] Add backend range-scan hooks to `PersistenceStore`.
- [x] Implement bounded scans for Memory, JSONL, libSQL, RocksDB, and
  PostgreSQL backends.
- [x] Preserve multiple event-sourced field mutations at one actor sequence
  across Memory/JSONL/libSQL/RocksDB/PostgreSQL.
- [x] Canonicalize same-sequence field ordering so cursor ordinals are stable
  across process restarts and backends.
- [x] Add batch event append semantics; Memory and RocksDB batch natively,
  libSQL/PostgreSQL use transactions, and runtime event-sourced state rolls
  back if persistence fails.
- [ ] Make workflow event + checkpoint publication one atomic durable commit.
- [ ] Introduce compaction-stable global commit identity.
- [ ] Benchmark cursor scan latency/allocation and set regression budgets.

### Phase 2 — typed indexes and query plan

- Define index declarations in AST/HIR.
- Add typed logical query plan independent of SQL.
- Keep SQL/libSQL as one physical executor.
- Add query diagnostics for scans and missing indexes.

### Phase 3 — incremental views

- Define `view` and `materialized view` semantics.
- Build delta operators for filter/map/join/group/reduce.
- Persist projection checkpoints/cursors.
- Make projection rebuild deterministic from the durable change stream.

### Phase 4 — analytical projection

- Define `relation` and analytical projection metadata.
- Add columnar segment writer/reader or an adapter to an embedded analytical
  engine.
- Add pruning, vectorized execution, spill, and object-store tiering.

### Phase 5 — distributed data runtime

- Partition ownership and rebalancing.
- Replica policies.
- Colocated transactional groups.
- Projection placement.
- Failure/recovery and deterministic simulation tests.

## Success criteria

The architecture is successful when an ordinary durable Nulang application can
use native state and queries without deploying a database, cache, CDC service,
stream broker, workflow engine, and analytical pipeline as separate mandatory
components, while still preserving escape hatches and interoperability for
workloads that genuinely require specialized external systems.
