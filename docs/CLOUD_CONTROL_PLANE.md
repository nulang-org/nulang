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

## Node allocation executor

`execute_pending_for_node` consumes the durable Start/Stop outbox for one node
through the provider-neutral `AllocationRuntime` boundary. The runtime adapter is
responsible for durably storing the local allocation epoch before reporting a
start or stop as successful. The executor then verifies the observed epoch and
ACKs the control-plane command only after the local mutation is proven.

The executor treats a newer local epoch as authoritative: delayed Start/Stop
commands from an older epoch are acknowledged without touching the newer
workload. A Stop installs a fence even when no workload is currently live, and
a delayed Start at an already-stopped epoch fails closed rather than
resurrecting that allocation. If a control-plane ACK fails after the runtime
action succeeds, the next pass observes the already-applied local epoch and
ACKs idempotently without repeating the runtime mutation.

This deliberately leaves process/WASM/microVM lifecycle details outside the
control-plane crate. Concrete node agents implement `AllocationRuntime` and
must make epoch fencing durable across their own restart.

## Reconciliation event loop

`ReconcileLoop` is the in-process orchestration layer above `reconcile_once`.
It does not create a second durable queue or invent durable identities. Callers
supply an event/evaluation identity from their durable event source; the loop
coalesces transient wakeups while durable evaluation, plan, ownership, fencing,
and outbox state remain in `ControlStore`.

The loop keeps at most one pending request per deployment. Newer desired
revisions supersede older pending revisions, stale revisions are ignored, and
multiple triggers for the same desired revision coalesce into one state-based
reconciliation turn. Processing order is deterministic by deployment id.

A failed turn remains queued with the exact same evaluation id. Retrying
therefore reaches `reconcile_once` with the same idempotency key instead of
manufacturing a new plan identity. `reconcile_ready` also requires an explicit
turn budget so an embedding controller retains backpressure and scheduling
control.

The resulting control-plane path is now:

```text
deployment / node / allocation / capacity event
                    |
                    v
             ReconcileLoop
                    |
                    v
             reconcile_once
                    |
                    v
   durable plan + epoch + outbox commit
                    |
                    v
        execute_pending_for_node
                    |
                    v
      fenced node-local workload state
```

## Next implementation slices

1. Concrete node-agent `AllocationRuntime` adapter for the first production execution target.
2. Fabric-backed service directory with generation-tagged health advertisements.
3. Workload identity and short-lived mTLS credentials bound to node/workload identity.
4. Capability-to-network-policy compilation, enforced in the NUL0 transport.
5. PostgreSQL state normalization only if measured contention/state size justifies it.
6. Optional L7 waypoint for HTTP/gRPC policy; no per-actor sidecars.

## Non-goals

- Embedding Nomad, Consul, or Istio into the Nulang runtime.
- Replacing the existing actor scheduler.
- Treating gossip state as authoritative allocation ownership.
- Transparent application-level retries of actor operations.
- Global consensus for ordinary regional scheduling decisions.
