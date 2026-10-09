# RFC 0030: Actor Resource Placement

- **Status:** Draft — Phase 1 implemented
- **Tier:** Experimental
- **Created:** 2026-09-20

## Summary

Nulang should treat placement as policy attached to an actor, while keeping provider inventory, pricing, leasing, and acquisition in the Nulang Cloud control plane.

Phase 1 adds an actor-specific policy adapter to `nulang-capacity`:

```text
ActorPlacementPolicy
        ↓
     JobSpec
        ↓
provider-neutral capacity broker
        ↓
AWS / GCP / Nebius / CoreWeave / RunPod / ...
```

The runtime does not gain cloud SDK dependencies and cloud adapters do not invent a second actor resource model.

## Motivation

Nulang already has two complementary pieces:

1. RFC 0017 defines placement as actor policy which may eventually target a local node, region, GPU host, edge runtime, or customer VPC while preserving actor identity.
2. `nulang-capacity` already normalizes provider inventory and ranks offers using hard resource constraints plus expected completed-job economics.

The missing piece is an explicit translation boundary between actor semantics and capacity scheduling.

Without that boundary, the language/runtime and Cloud control plane are likely to evolve duplicate schemas for CPU, memory, GPU, region, trust, interruption, and recovery policy.

## Phase 1 model

### Continuity class

`ActorContinuity` describes the recovery economics of an activation:

```text
Ephemeral -> WorkloadClass::Ephemeral
Durable   -> WorkloadClass::Checkpointable
Critical  -> WorkloadClass::Critical
```

This is intentionally about placement/recovery policy, not actor language role. A workflow, agent, entity, or ordinary actor can choose whichever continuity class matches its operational contract.

Defaults are conservative:

- ephemeral: interruptible capacity allowed;
- durable: interruptible capacity allowed with a default checkpoint interval;
- critical: interruptible capacity rejected by default.

The policy remains explicit and can be overridden by a control plane when an operator deliberately accepts a different tradeoff.

### Resource requirements

`ActorResources` currently describes hard minimums:

```text
vCPU
memory
accelerator model
accelerator count
VRAM per accelerator
```

These are eligibility constraints, not score hints. A host that cannot satisfy them must never win by being cheaper.

### Placement constraints

`ActorPlacementPolicy` adds:

```text
architecture
allowed regions
minimum trust tier
interruptibility policy
expected active compute time
checkpoint cadence
restart overhead
expected egress
```

The adapter validates the policy and produces the existing provider-neutral `JobSpec`. Concrete providers remain outside this layer.

## Why this lives in `nulang-capacity`

The core runtime should not depend on provider economics or cloud inventory. Its job is to execute actors on a selected node and preserve actor identity/message semantics.

The capacity crate already owns:

- hard eligibility filtering;
- cost-to-complete scoring;
- interruption economics;
- provider health;
- durable placement claims;
- idempotent leasing;
- provider-neutral adapters.

Therefore the actor-to-capacity adapter belongs beside that broker rather than in `src/runtime`.

A future compiler/runtime metadata format may serialize the same actor policy schema, but provider selection should still happen in a control-plane component.

## Example future language surface

This RFC does not freeze syntax. A future surface could lower to the normalized policy:

```nulang
@resources(cpu = 4, memory = 16.GiB, gpu = H100(count = 1, vram = 80.GiB))
@placement(region = ["us-east", "us-central"], continuity = durable)
agent VisionAgent {
  ...
}
```

The important part is the semantic lowering, not the annotation spelling.

## Placement groups

Ray-style placement groups are useful for coordinated actor teams, but they are deliberately deferred.

Correct PACK/SPREAD semantics require a topology-aware inventory of concrete allocatable hosts or resource bundles. Today's `CapacityOffer` primarily models provider offers/capacity economics; pretending that offer types are individual hosts would produce misleading group placement guarantees.

A later phase should first introduce a concrete allocatable-resource identity/topology model, then support:

```text
PACK
SPREAD
STRICT_PACK
STRICT_SPREAD
```

for actor teams, model shards, and multi-resource agent workloads.

## Data locality

Phase 1 uses expected egress as an economics input. Later placement should integrate the shared object store so scheduling can account for where large immutable objects, model shards, embeddings, tensors, or checkpoints already reside.

The intended model is:

```text
actor resource constraints
+ object/data locality
+ trust/region constraints
+ observed throughput
+ interruption/recovery cost
= placement decision
```

## Compatibility

This phase is additive:

- no language syntax changes;
- no runtime dependency on `nulang-capacity`;
- no provider adapter changes;
- no change to existing `JobSpec` semantics;
- existing broker ranking remains the single provider-selection algorithm.

## Follow-up

1. Serialize actor placement policy in Cloud deployment metadata.
2. Connect actor/entity activation requests to capacity placement claims.
3. Feed actor checkpoint/journal properties into interruption policy automatically where safe.
4. Add object-store locality to scoring.
5. Introduce concrete host/resource-bundle topology.
6. Implement Ray-like placement groups on top of that topology.
7. Add migration/rebalancing decisions that preserve stable logical actor identity.
