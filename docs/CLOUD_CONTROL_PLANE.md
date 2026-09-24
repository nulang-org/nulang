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

The planner is deliberately pure. It does not provision machines, acquire
capacity, start actors, stop actors, or write durable state.

## Fencing

Every logical `(deployment_id, replica)` has a monotonically increasing
allocation epoch. New placements use `max(observed epoch) + 1`. A lower-epoch
allocation is considered superseded even if it still reports `Running`.

The commit/execution layer must enforce the same epoch so delayed commands or
partitioned zombie workers cannot regain ownership.

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

## Next implementation slices

1. Durable evaluation/plan/allocation store with compare-and-set epoch commit.
2. Reconciler state machine that consumes deployment/node/allocation changes.
3. Runtime allocation executor honoring epoch fencing and drain/stop commands.
4. Fabric-backed service directory with generation-tagged health advertisements.
5. Workload identity and short-lived mTLS credentials bound to node/workload
   identity.
6. Capability-to-network-policy compilation, enforced in the NUL0 transport.
7. Optional L7 waypoint for HTTP/gRPC policy; no per-actor sidecars.

## Non-goals

- Embedding Nomad, Consul, or Istio into the Nulang runtime.
- Replacing the existing actor scheduler.
- Treating gossip state as authoritative allocation ownership.
- Transparent application-level retries of actor operations.
- Global consensus for ordinary regional scheduling decisions.
