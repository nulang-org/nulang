# RFC 0022: Logical Actor Identity and Activation Handles

- **Status:** Draft — Phase 1 implemented
- **Tier:** Experimental
- **Created:** 2026-09-20
- **Depends on:** RFC 0016 (virtual actors), RFC 0017 (unified runtime primitives)

## Summary

Nulang virtual actors need separate logical and physical identities.

- `GrainId` is the lossless logical identity: `(actor type, key)`.
- `LogicalActorId` is a stable 128-bit BLAKE3-derived digest for compact directory, persistence, tracing, and wire metadata.
- `ActivationHandle` is an ephemeral 48-bit runtime-local handle compatible with the current NaN-boxed `ActorRef` representation.
- `ActivationDirectory` maintains a bijection between full `GrainId` values and live activation handles.

The existing `grain_actor_id` 48-bit FNV mapping remains unchanged for compatibility, but is explicitly legacy-only and must not become the durable identity boundary.

## Motivation

The current runtime derives a virtual actor's live actor id by hashing `(grain_type, key)` and truncating to 48 bits. That couples durable identity to a VM representation detail and creates a birthday-collision risk at large actor populations.

The required invariant is:

```text
logical identity != activation handle != node id != process id
```

A logical actor must retain the same identity through dehydration, rehydration, migration, node failure, and future runtime representation changes.

## Identity model

### Full logical identity

`GrainId` remains authoritative. Its canonical encoding is length-prefixed so no separator ambiguity is possible.

```text
u32(type_len) || type_utf8 || u32(key_len) || key_utf8
```

### Compact logical identity

`LogicalActorId` is derived as:

```text
BLAKE3("nulang.logical-actor.v1\0" || GrainId.canonical_bytes())[0..16]
```

The 128-bit digest is an index/protocol representation, not a license to discard the full `GrainId`. Systems using the digest must retain or recover the full identity so a theoretical hash collision cannot silently alias two entities.

### Activation handle

`ActivationHandle` occupies the current `ActorRef` payload range:

```text
1 ..= 0x0000_FFFF_FFFF_FFFF
```

Activation handles are runtime-local, may change after reactivation/migration, and must not be used as the sole durable persistence key.

## Phase 1 implemented

- Add canonical `GrainId` encoding.
- Add 128-bit `LogicalActorId`.
- Add `ActivationHandle` with the current VM payload bound.
- Add collision-free `ActivationDirectory` keyed by full `GrainId`.
- Preserve the historical `grain_actor_id` encoding exactly.
- Add regression tests for canonical identity, stable logical IDs, handle bounds, directory bijection, and legacy-ID stability.

## Follow-up phases

### Phase 2 — runtime integration

- Add `ActivationDirectory` to `Runtime`.
- Make `Grain.ref` allocate/resolve activation handles instead of deriving them from `grain_actor_id`.
- Route `send_to_grain`, hydration, dehydration, and eviction through the directory.
- Preserve a compatibility resolver for existing snapshots keyed by legacy 48-bit ids.

### Phase 3 — persistence migration

- Introduce a persistence key that stores full logical identity or its versioned canonical representation.
- Dual-read legacy `u64` snapshot/journal keys during migration.
- Stop requiring activation-handle stability across restarts.

### Phase 4 — distributed entity directory

- Map logical identity to node, activation generation, and local activation handle.
- Fence stale routes using activation generations/epochs.
- Re-resolve after node failure or migration rather than assuming actor-number permanence.

### Phase 5 — wire addressing

Remote virtual-actor envelopes should carry logical recipient identity plus optional cached activation routing metadata, protocol identity/version, message id, trace context, and deadline.

## Compatibility

This RFC's first phase is additive. It does not change the NaN-boxed value ABI, persistence formats, NUL0 wire encoding, or current `grain_actor_id` behavior.

## Invariants

1. Two distinct live `GrainId` values never share an `ActivationHandle` in one directory.
2. Every registered handle reverse-resolves to exactly one full logical identity.
3. Dehydration or migration may change an activation handle without changing logical identity.
4. Persistence correctness must not depend on reproducing a prior activation handle.
5. The legacy 48-bit FNV mapping remains byte-for-byte stable during the compatibility window.
6. Distributed routing must eventually fence stale activations with generations/epochs.

## Why not widen ActorRef immediately?

The VM currently stores actor refs inside a NaN-boxed value with a 48-bit payload. Replacing that representation would affect VM, JIT/AOT/WASM, FFI, persistence, and wire compatibility. Identity indirection fixes the semantic problem now and leaves widening as an independent optimization project.