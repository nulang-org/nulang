---
title: Fewer Services for Durable Distributed Applications
description: How Nulang aims to unify actors, durable workflows, messaging, streams, coordination, and storage—and the roadmap toward Redis, NATS, Kafka, Temporal, and database workloads.
---


A distributed application often grows into a collection of **application services, a cache, a message broker, a durable event log, a workflow engine and a database**. Redis, NATS, Kafka, Temporal and PostgreSQL each solve important problems, but connecting them introduces extra deployment, security, observability, backup, failure and data-consistency boundaries.

**Nulang's long-term goal is not merely to provide client SDKs for this stack.** It is to put actors, supervised execution, durable state, messaging, streams and—eventually—general-purpose data services on a coherent runtime substrate. In workloads where those semantics overlap, that could reduce the **number of independently operated systems**.

**Status: experimental roadmap, not a replacement announcement.** Nulang is alpha software. Its infrastructure components are at different levels of completion, and it does **not** currently offer drop-in or production-equivalent replacements for Redis, NATS, Kafka, Temporal or a general-purpose database.

## The infrastructure Nulang is trying to consolidate

| Software teams commonly deploy | Why it is used | Relevant Nulang work today | What remains before replacement is credible |
| --- | --- | --- | --- |
| **Redis** | Cache, counters, pub/sub, streams, coordination | Actor-local state, CRDT runtime primitives; Fabric topic routing and durable stream groundwork | Distributed expiring maps, cache semantics, atomic counters/CAS, leases, proven failover, a tested RESP compatibility subset |
| **NATS / JetStream** | Subjects, fan-out, queue groups and persisted messages | Experimental Fabric subscriptions, cross-node routing, consumer selection and quorum-oriented stream replication | Production operations, consumer ACK/redelivery, retention, interoperability and tested JetStream-equivalent guarantees |
| **Apache Kafka** | Partitioned replicated logs, consumer replay and event pipelines | File-backed segmented logs, checksums, durable consumer cursors, replication and committed reads | Multi-partition throughput, retention, consumer-group coordination, replay/indexing, broker compatibility and operational parity |
| **Temporal** | Durable workflow progress, timers, signals and retries | Persistent actors, checkpoint/journal recovery, reference-runtime workflow steps; early Temporal compatibility adapter | Complete workflow backend parity, reliable external-effect semantics, SDK/server API coverage and production-grade recovery/operations |
| **Databases** (eventually) | Durable transactions, queries, indexes and shared data | Pluggable existing persistence stores; **NuDB** prototype with WAL, tablet ownership/fencing, MVCC/checkpoint building blocks | Transactional SQL/query engine, indexing, replicated consensus, multi-tablet transactions, migrations, ecosystem and backup/restore guarantees |

This is a **capability-by-capability direction**, not a pledge that every application should stop using those systems. Kafka-style analytics pipelines, mature SQL applications and operationally sensitive workloads can continue to benefit from specialized services.

## What is actually implemented?

### Persistent actors and durable execution

On the reference bytecode/runtime path, Nulang supports **opt-in persistent actor state**, journals, snapshots, state restoration, supervision and some workflow progress recovery. Storage can be file-backed or use optional SQLite/libSQL, RocksDB or PostgreSQL backends.

That already makes it possible to explore applications where **the same runtime owns computation and recovery state**, rather than passing every state transition through an additional workflow service. External side effects are not automatically exactly once, and the portable WASM backend is **not yet workflow-equivalent**.

Read [durable workflow semantics](/workflows/overview/) and the [current implementation status](https://github.com/nulang-org/nulang/blob/main/docs/IMPLEMENTATION_STATUS.md).

### Fabric: from actor messages toward brokers

[Nulang Fabric](https://github.com/nulang-org/nulang/blob/main/docs/FABRIC.md) extends the actor runtime with experimental wildcard topic subscriptions, fan-out, queue-group selection, cross-shard/remote routing and admission/backpressure reporting. Its separate stream layer adds append-only segmented logs, checksums, durable consumer cursors, replication intent, replica ACKs, and quorum-oriented committed records.

**Fabric topics are ephemeral; Fabric streams are the durable-log path.** Neither should be described as a drop-in NATS or Kafka broker. Consumer delivery contracts, retention policies, generalized partition management and compatibility gateways still need work.

Read the [Fabric Streams implementation notes](https://github.com/nulang-org/nulang/blob/main/docs/FABRIC_STREAMS.md).

### NuDB: research toward an integrated database

NuDB is an **early database-engine prototype** with WAL-backed tablet primitives, write fencing, MVCC and checkpoint/recovery tests. Its aim is to enable durable data services that are close to actors and workflows without making every hot query or storage loop an actor.

It is **not yet a general SQL database or a PostgreSQL replacement**. A database replacement would need much more than durable bytes: query planning, transactions, indexing, replication, migrations, consistent backups and a stable client protocol are all part of the product contract.

See the [database prototype source](https://github.com/nulang-org/nulang/tree/main/src/database).

## The direction: one substrate, fewer operational boundaries

A possible future architecture is:

1. **Typed actors and effects** own behavior, state access and capability boundaries.
2. **Durable transitions** coordinate state changes, timers, effects and outbox-style messaging with explicit crash/replay contracts.
3. **Fabric topics and streams** provide local-to-distributed communication and durable log functions where those semantics fit.
4. **Integrated data primitives / NuDB** provide more transactional and query workloads over time.
5. **Compatibility adapters** let existing Redis, NATS or Temporal clients migrate incrementally where supported—not by re-implementing every unrelated feature in the language core.

If successfully validated, a small durable service might need **a Nulang runtime and durable storage** instead of a separate task queue, workflow engine and bespoke recovery infrastructure. This is a potential architectural simplification, **not a demonstrated cost saving or production SLA**.

## The hard engineering work cannot be skipped

Consolidation is only valuable if it **reduces total complexity**. A unified runtime can also become a larger failure domain. Before replacing a dedicated infrastructure service, Nulang needs evidence for:

- Correctness under process kills, partitions, stale leaders, duplicate delivery and disk corruption.
- Consumer delivery, retention, replication and transaction contracts at realistic load.
- Backup/restore, upgrades, security isolation, observability and incident recovery.
- Fair, reproducible throughput and latency tests against the specialized alternative.
- Compatibility at the protocol edge, or a documented migration path when semantics differ.

For now, **keep Redis, NATS, Kafka, Temporal and your database when your application depends on their established guarantees**. Nulang is an experimental path toward requiring fewer of them for selected future workloads.

## Questions developers ask

### Can Nulang replace Redis today?

Not generally. Fabric and CRDT work provide some related primitives, but a production cache, distributed counters, eviction and RESP compatibility are not complete.

### Can Nulang replace NATS or Kafka today?

No. There is substantial routing and durable stream groundwork, but complete client/broker interoperability and established operational semantics have not been demonstrated.

### Can Nulang replace Temporal?

Not as a production-compatible Temporal service today. The reference runtime has durable-workflow primitives and an experimental, protocol-neutral Temporal adapter. Full SDK and server compatibility are future work.

### Is Nulang building a database?

Yes—**NuDB is in progress** as a prototype. Replacing general-purpose databases is a longer-term research and engineering goal, not a shipped capability.

### Does fewer services mean zero infrastructure?

No. Storage, backups, networking, security, observability and deployment still exist. The goal is to **consolidate overlapping application-runtime responsibilities**, not eliminate the need for infrastructure.

## Explore the language and roadmap

- [Compare Nulang with Erlang, Elixir, Go, Rust, Akka, Orleans and Ray](/comparisons/)
- [Actor model](/actors/overview/) · [Distribution](/actors/distribution/) · [Durable workflows](/workflows/overview/)
- [Fabric roadmap](https://github.com/nulang-org/nulang/blob/main/docs/FABRIC.md) · [NuDB prototype](https://github.com/nulang-org/nulang/tree/main/src/database) · [Temporal adapter design](https://github.com/nulang-org/nulang/blob/main/RFC/0025-temporal-compatibility.md)

For context on the systems Nulang draws from, see the official [Redis Streams](https://redis.io/docs/latest/develop/data-types/streams/), [NATS JetStream](https://docs.nats.io/concepts/jetstream) and [Temporal](https://docs.temporal.io/temporal) documentation.
