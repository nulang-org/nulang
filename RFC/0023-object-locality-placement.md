# RFC 0023: Object-Locality-Aware Placement

- **Status:** Draft
- **Created:** 2026-09-16
- **Depends on:** RFC 0016 virtual actors and object store; RFC 0021 actor capacity placement

## Summary

Nulang placement should account for where large immutable objects already exist. CPU/GPU price alone is an incomplete placement signal for actors that depend on model weights, tensors, embeddings, media, snapshots, or other large object-store values.

This RFC adds a provider-neutral locality scoring layer to `nulang-capacity`.

## Phase 1 scope

The current `CapacityOffer` model describes provider/region/zone capacity, not concrete hosts or memory tiers. Therefore Phase 1 models locality at the same level:

- provider
- region
- optional zone

A region-scoped replica is local to any offer in the same provider/region. A zonal replica is local only to a matching zonal offer.

RAM, VRAM, mmap, NVMe, and exact-node locality are deferred until concrete host topology exists.

## Data model

A placement request may carry immutable object dependencies:

```text
ObjectDependency {
  object_id
  size_bytes
  replicas[]
  remote_transfer_usd_per_gib
  remote_transfer_seconds_per_gib
}
```

Transfer economics belong to the object/control-plane observation, not the destination compute offer. This avoids incorrectly treating the destination provider's compute egress price as the cost to fetch remote data.

## Scoring

For each eligible capacity offer:

1. Run the existing compute/interruption/startup score.
2. For every required object, check for a local replica.
3. Missing local replicas contribute their full materialization size.
4. Add observed transfer dollars directly to effective cost.
5. Convert materialization time through the existing startup-seconds weight.
6. Keep deterministic provider/offer tie breaking.

Thus locality can outweigh a nominally cheaper remote GPU when moving the working set costs more than the compute savings.

## Why immutable objects first

RFC 0016 already treats large immutable values as object references rather than actor-message copies. Immutable objects are the safest data-locality primitive because replicas can be cached and moved without synchronization semantics for mutable shared state.

## Future phases

### Concrete host topology

Add host/resource-bundle identity so placement can distinguish:

- local RAM
- GPU VRAM
- mmap/page cache
- local NVMe
- same rack/zone
- regional object store
- remote region/provider

### Placement groups

Only after concrete host topology exists should Nulang add Ray-like group strategies:

- PACK
- SPREAD
- STRICT_PACK
- STRICT_SPREAD

Those guarantees are not meaningful when the scheduler sees only provider capacity offers.

### Replica planning

The scheduler can later choose between:

- moving the actor to the data;
- materializing data near the actor;
- prewarming a replica;
- pinning a hot object;
- spilling cold objects to cheaper storage.

## Decision

Object locality is a placement-policy input, not a new actor primitive. `nulang-capacity` owns provider-neutral locality economics; the object store owns replicas; provider adapters own the mechanics of transfer/materialization.
