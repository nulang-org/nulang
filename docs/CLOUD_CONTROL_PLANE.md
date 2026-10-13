# Nulang Cloud Control Plane

**Status:** Experimental foundation implemented in `crates/nulang-cloud-control`.

This document describes the current implementation boundary. The historical
`docs/archive/DESIGN_CLOUD.md` remains design context, not implementation truth.

## Why this layer exists

The actor runtime scheduler and the Cloud scheduler solve different problems:

- `src/runtime/scheduler.rs` chooses which runnable actor executes next on one
  runtime shard.
- `nulang-cloud-control` decides which cluster node should own a deployment
  replica.

Keeping those layers separate prevents cloud policy, provider APIs, and
reconciliation state from entering the language/runtime hot path.

## Current planning pipeline

```text
DeploymentSpec + Evaluation + NodeSnapshot + ObservedAllocations
                         |
                         v
                retain/fence analysis
                         |
                         v
                 hard feasibility
       state / arch / region / zone / trust
       labels / capabilities / resources
       max replicas per node
                         |
                         v
                  soft ranking
      region locality / zone spread / node spread
                  resource headroom
                         |
                         v
                   PlacementPlan
       retained / placements / superseded / blocked
```

The planner is deliberately pure. It does not provision machines or mutate
runtimes. Durable ownership is handled by the separate `ControlStore` commit
boundary described below.

## Fencing

Every logical `(deployment_id, replica)` has a monotonically increasing
allocation epoch. New placements use `max(observed epoch) + 1`. A lower-epoch
allocation is considered superseded even if it still reports `Running`.

The commit/execution layer must enforce the same epoch so delayed commands or
partitioned zombie workers cannot regain ownership.

Node suspicion is deliberately **not** a fencing event. An allocation on a
`Suspect`, `Unreachable`, or temporarily absent node remains authoritative;
the planner will not create a replacement epoch until membership explicitly
marks the node `Removed` or an operator places it in `Draining`. This mirrors
the runtime's confirmed-removal safety rule and trades temporary availability
for prevention of two live owners during a partition.

## Explainable placement

Hard constraints are evaluated before ranking. If no node is feasible the plan
contains:

- unscheduled replica indexes;
- every considered node and its rejection reasons;
- aggregate rejection counts suitable for `nula cloud explain`.

A partially satisfiable deployment returns the placements that are safe to make
plus an explicit blocked remainder. The later durable reconciler decides when a
partial plan may be committed.

## Relationship to existing Cloud work

`crates/nulang-capacity` remains responsible for provider-neutral capacity
offers, economic ranking, provider health, leases, and the durable anti-double-
placement claim boundary.

The intended composition is:

```text
nulang-capacity
    provision/lease candidate infrastructure
              |
              v
nulang-cloud-control
    reconcile desired workload placement
              |
              v
durable plan/epoch commit
              |
              v
Nulang runtime / Fabric
    execute allocation
```

Provider capacity selection and workload placement are intentionally distinct:
the cheapest VM offer is not necessarily the correct node for an existing
stateful or locality-sensitive workload.

## Durable commit and reconciliation

The control-plane crate now exposes a storage-neutral `ControlStore` contract.
An evaluation is persisted before planning, then `commit_plan` atomically:

1. verifies that the evaluation identity/revision still match;
2. compare-and-sets the current allocation epochs used by the plan;
3. marks superseded allocations stopped;
4. installs replacement allocations in `Starting` state;
5. persists the exact `PlacementPlan`;
6. writes deterministic Start/Stop commands to a durable outbox.

Exact retries of one evaluation return `AlreadyCommitted`; reusing an
evaluation id with different content fails closed. A stale plan that lost an
epoch race cannot mutate allocations or enqueue commands.

`MemoryControlStore` is provided for deterministic tests. The
`JsonFileControlStore` is a single-process durable backend: every mutation is
serialized to a sibling temporary file, fsynced, renamed over the previous
state, and the parent directory is fsynced on Unix. It is suitable for local
controllers and recovery tests, not multi-controller production deployment.
For multi-controller deployments the optional `postgres` feature provides
`PostgresControlStore`. Each scheduling region/cell uses a named scope row.
Mutations run inside a PostgreSQL transaction and lock that row with
`SELECT ... FOR UPDATE`; epoch validation, plan persistence, allocation
ownership, and outbox updates therefore serialize across controller processes.
The row also carries a monotonic version for diagnostics.

The PostgreSQL backend deliberately stores one serialized control-state document
per scope in the first implementation. This minimizes schema and migration
surface while preserving transactional correctness. If profiling later shows
row contention or state-size pressure, the contract can be normalized into
separate tables without changing scheduler/reconciler semantics.

`reconcile_once` provides the first idempotent reconciler turn. Recovery of an
already committed evaluation returns the durable plan rather than recomputing it
against a newer node snapshot.

## Durable command execution

`dispatch_pending` drains the allocation outbox through an
`AllocationCommandSink`. The sink contract is explicitly idempotent because a
controller can crash after a node applied a command but before the ACK reached
durable storage.

Execution ordering is safety-biased:

- Stop commands are attempted before Start commands;
- a failed Stop blocks Starts for the same logical deployment replica;
- a pending Start is rechecked against the authoritative allocation epoch;
- a stale Start is never delivered as Start; it is converted to an idempotent
  compensating Stop before the stale command is acknowledged;
- after a successful Start, authority is checked again; if it changed during
  delivery, a compensating Stop is issued before ACK.

This closes the crash-after-start/stale-outbox recovery case without requiring
the executor to infer intent from gossip. Dispatchers now acquire durable leases
with monotonically increasing claim generations before delivery. An unresolved
Stop blocks a Start for the same logical replica at the store boundary, and a
dispatcher whose lease has been reclaimed cannot ACK or release the newer
generation. The same claim state is persisted by JSON and PostgreSQL stores.

## Durable node-side execution fence

`FencedNodeExecutor` is the concrete node-side implementation of the
allocation-command sink boundary. It persists a small local lifecycle record per
logical deployment replica and applies the allocation epoch again at the point
where external side effects occur.

The state machine is intentionally conservative:

```text
Start(N)
  -> persist Starting(N)
  -> idempotent WorkloadLifecycle.start(N)
  -> persist Running(N)

Stop(N)
  -> persist Stopping(N)
  -> idempotent WorkloadLifecycle.stop(N)
  -> persist Stopped(N)
```

A retryable launcher failure leaves `Starting` or `Stopping` durable so the
exact operation is retried after controller/node-agent recovery. A newer Start
cannot pass a locally live prior epoch. A delayed Start for an already-fenced
older epoch is a safe no-op, while the same stopped epoch cannot be resurrected.
A delayed Stop for an old epoch may clean up that exact orphaned workload but
cannot overwrite the durable record for a newer running epoch. Commands for a
different node fail before any launcher side effect.

The node execution file is a **secondary fence**, not cluster ownership. The
transactional control store remains authoritative. The executor serializes
commands through one node-local state machine and requires
`WorkloadLifecycle` implementations to make exact Start/Stop identities
idempotent.

## Immutable workload revision contract

Actual Start execution now has a separate immutable admission identity instead
of resolving a deployment revision through a mutable path, image tag, or URL.

`WorkloadRevisionSpec` binds one `(deployment_id, revision)` to:

- package name/version;
- RFC 0020 artifact kind and compiler-derived artifact id;
- BLAKE3 digest of the exact executable bytes;
- BLAKE3 digest of the canonical behavior manifest;
- admitted target / ABI / backend;
- ordered application arguments;
- names of required runtime configuration values.

Configuration **values and secrets are not part of the revision document**.
Artifact locations are also deliberately absent: a content-addressed mirror can
move while workload identity remains unchanged.

The complete revision document has its own domain-separated canonical BLAKE3
digest. `ControlStore::register_workload_revision` persists the normalized
document through Memory, JSON, and PostgreSQL backends. Exact registration
retries are idempotent; reusing the same deployment revision with different
artifact/manifest/launch identity fails closed.

`RevisionBoundLifecycle` bridges that registry to the node execution fence.
A Start does not reach `ResolvedWorkloadLifecycle` until its immutable
revision exists and validates. A Stop bypasses revision lookup so fencing and
cleanup remain possible during registry or artifact-store outages.

The control plane still does not fetch executable bytes itself. A concrete
launcher/CAS adapter must fetch by digest, verify the exact artifact and RFC
0020 behavior manifest, enforce target/ABI admission, then launch it. This keeps
artifact transport replaceable without weakening deployment identity.

## Next implementation slices

1. Content-addressed artifact/behavior-manifest fetch + verification adapter.
2. Workload identity and short-lived mTLS credentials bound to node/workload
   identity.
3. Capability-to-network-policy compilation, enforced in the NUL0 transport.
4. Optional L7 waypoint for HTTP/gRPC policy; no per-actor sidecars.
5. PostgreSQL state normalization only if measured contention/state size justifies it.

## Non-goals

- Embedding Nomad, Consul, or Istio into the Nulang runtime.
- Replacing the existing actor scheduler.
- Treating gossip state as authoritative allocation ownership.
- Transparent application-level retries of actor operations.
- Global consensus for ordinary regional scheduling decisions.
