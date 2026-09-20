# RFC 0022: Logical Actor Identity and Activation Handles

- **Status:** Draft — Phase 1 + local activation fencing substrate implemented
- **Tier:** Experimental
- **Created:** 2026-09-20
- **Depends on:** RFC 0016 (virtual actors), RFC 0017 (unified runtime primitives)

## Summary

Nulang virtual actors need separate logical and physical identities.

- `GrainId` is the lossless logical identity: `(actor type, key)`.
- `LogicalActorId` is a stable 128-bit BLAKE3-derived digest for compact directory, persistence, tracing, and wire metadata.
- `ActivationHandle` is an ephemeral 48-bit runtime-local handle compatible with the current NaN-boxed `ActorRef` representation.
- `ActivationEpoch` is a monotonic incarnation number for successive activations of the same full logical identity.
- `ActivationDirectory` maintains a bijection between full `GrainId` values and live activation handles and locally fences stale activation stamps.

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
u64(type_len) || type_utf8 || u64(key_len) || key_utf8
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

### Activation epoch

Every live activation also carries an `ActivationEpoch`, starting at 1. Repeated
resolution of the same live activation preserves its epoch. Removing and later
reactivating the same full `GrainId` advances the epoch monotonically.

The local authority token is therefore the full logical identity plus
`(ActivationHandle, ActivationEpoch)`. A delayed operation carrying an older
stamp is stale even when the logical actor identity is unchanged.

`ActivationDirectory::is_current` enforces this inside one runtime process.
Cross-process or cross-node enforcement is deliberately not claimed until the
epoch is stored in the durable/distributed directory. Epoch history is keyed by
the full `GrainId`, not solely by the compact 128-bit digest.

### External authority handoff

The local directory now also exposes an explicit bridge for a future Cloud or
distributed ownership service:

- `observe_authoritative_epoch(grain, epoch)` records a **strictly newer**
  externally established epoch, invalidates any older live local activation,
  and places the logical identity behind an external-authority fence.
- while fenced, `resolve_or_activate` fails with `AuthorityRequired` instead of
  autonomously inventing the next epoch;
- `install_authoritative_activation(grain, epoch)` installs an epoch explicitly
  granted by the ownership layer and rejects a grant older than the highest
  epoch already observed.

This is deliberately a **handoff API, not a consensus protocol**. The local
runtime does not decide which node owns a logical actor. In particular, two
nodes claiming the same epoch cannot be resolved by `ActivationDirectory`;
lease/quorum/consensus policy above the runtime must select the owner before an
activation grant is installed. Once a strictly newer epoch is observed, local
work from an older epoch fails closed.

## Phase 1 implemented

- Add canonical `GrainId` encoding.
- Add 128-bit `LogicalActorId`.
- Add `ActivationHandle` with the current VM payload bound.
- Add collision-free `ActivationDirectory` keyed by full `GrainId`.
- Add local monotonic `ActivationEpoch` / `ActivationStamp` fencing across deactivation and reactivation.
- Add an external-authority handoff that can fence an older local incarnation and require an explicit ownership grant before reactivation.
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

- Persist the authoritative ownership record keyed by full logical identity (or a collision-checked canonical representation), not the legacy 48-bit actor id.
- Advance the ownership epoch only through the authoritative control-plane handoff and feed that grant into `install_authoritative_activation`.
- Propagate strictly newer epochs to runtimes through `observe_authoritative_epoch` so stale local activations are invalidated before further work is admitted.
- Carry the authoritative epoch on routes and durable commits and reject stale epochs at the persistence boundary.
- Define an explicit equal-epoch conflicting-owner rule in the ownership service; local first-writer/arrival order is not sufficient distributed arbitration.
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
6. A reactivated full logical identity receives an epoch strictly greater than its prior local incarnation.
7. A stale local activation stamp is rejected once a replacement activation becomes authoritative.
8. Once a runtime observes a strictly newer externally authoritative epoch, autonomous local reactivation is forbidden until an explicit authority grant is installed.
9. An authority grant older than the highest observed epoch is rejected.
10. Equal-epoch conflicting node claims are not resolved locally; the distributed ownership layer must select one owner before granting activation authority.
11. Distributed routing and durable commits must eventually carry and enforce the authoritative epoch.

## Why not widen ActorRef immediately?

The VM currently stores actor refs inside a NaN-boxed value with a 48-bit payload. Replacing that representation would affect VM, JIT/AOT/WASM, FFI, persistence, and wire compatibility. Identity indirection fixes the semantic problem now and leaves widening as an independent optimization project.