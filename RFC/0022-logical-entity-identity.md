# RFC 0022: Logical Entity Identity and Activation Handles

- **Status:** Draft — Phase 1 groundwork implemented
- **Tier:** Experimental
- **Created:** 2026-09-16
- **Depends on:** RFC 0016 (virtual actors), RFC 0017 (unified runtime primitives)

## Summary

Virtual actors need two different identities:

1. **Logical identity** — the durable `(entity type, key)` identity that survives process restarts, node changes, migration, dehydration, and software upgrades.
2. **Activation handle** — a compact runtime-local handle used to address a currently live activation efficiently.

Nulang historically derived both from `grain_actor_id(GrainId)`, a deterministic hash truncated to the 48-bit payload available in the NaN-boxed `ActorRef` representation. That is convenient but it conflates a representation detail with durable identity and introduces birthday-collision risk at large populations.

This RFC makes `GrainId` authoritative. A compact `ActivationHandle` is explicitly ephemeral and resolved through an `ActivationDirectory`.

## Motivation

A durable virtual actor may exist for years while its activations come and go many times. The following must remain true:

```text
logical entity identity != process id != node id != activation handle
```

A 48-bit truncated hash cannot safely serve as a globally durable identity at large scale. Under a uniform-hash assumption, the probability that at least one collision exists is approximately:

| Population | Collision probability |
|---:|---:|
| 1,000,000 | 0.18% |
| 10,000,000 | 16.3% |
| 20,000,000 | 50.9% |
| 30,000,000 | 79.8% |

The exact hash distribution can change these numbers, but not the architectural conclusion: a compact runtime handle is the wrong durability boundary.

## Semantic model

### Logical identity

`GrainId` remains the canonical logical entity identity:

```rust
pub struct GrainId {
    pub grain_type: String,
    pub key: String,
}
```

Equality uses the complete pair. Persistence, distributed directory records, migration metadata, deduplication state, and observability must eventually key by this logical identity (or by a lossless/versioned encoding of it), not by a truncated actor handle.

`GrainId::canonical_bytes()` provides an unambiguous length-prefixed encoding for hashing, signatures, storage adapters, and directory protocols. A hash of these bytes may be used as an index accelerator, but collisions must be resolved against the complete logical identity.

### Activation handle

`ActivationHandle` is a compact runtime-local identifier:

```rust
pub struct ActivationHandle(u64);
```

For compatibility with the current NaN-boxed `ActorRef`, values are restricted to `1..=0x0000_FFFF_FFFF_FFFF`. Zero remains invalid/null.

Activation handles are:

- unique within an activation directory;
- cheap to embed in the existing VM value representation;
- allowed to change after restart, migration, eviction, or reactivation;
- never sufficient as the sole persistence identity of a virtual actor.

### Activation directory

Each runtime owns a bijection:

```text
GrainId <-> ActivationHandle
```

The Phase 1 `ActivationDirectory` allocates handles monotonically and maintains reverse lookup. This removes hash collisions from live local addressing without changing the VM ABI.

The future distributed directory extends the same semantics:

```text
GrainId
   |
   v
Distributed Entity Directory
   |
   +--> activation node
   +--> activation generation
   +--> local ActivationHandle
   +--> protocol/code version
```

## Compatibility migration

This change deliberately does not rewrite persistence or network formats in one step.

### Phase 1 — groundwork (implemented in this RFC branch)

- Add `ActivationHandle`.
- Add `ActivationDirectory` with bijective allocation/reverse lookup.
- Add canonical `GrainId` encoding.
- Document `grain_actor_id` as a compatibility-only legacy mapping.
- Preserve the existing 48-bit ActorRef ABI.

### Phase 2 — runtime integration

- Add an `ActivationDirectory` field to `Runtime`.
- Route `Grain.ref`/`send_to_grain` through the directory.
- Stop deriving new live virtual-actor handles from `grain_actor_id`.
- Preserve a compatibility resolver for snapshots written by the legacy scheme.

### Phase 3 — persistence key migration

- Introduce a persistence key type that can represent full logical entity identity.
- Store snapshots/journals by `GrainId`, not activation handle.
- Add versioned migration for legacy `u64` grain snapshot keys.
- Record both logical identity and legacy key during the compatibility window.

### Phase 4 — distributed directory

- Replicate/partition logical identity ownership independently of activation handles.
- Include activation generation to reject stale routes.
- Re-resolve after node failure/migration instead of assuming a permanent actor number.
- Cache directory lookups, but validate cached generation/lease information.

### Phase 5 — wire protocol

Remote virtual-actor messages should address a logical entity directly or carry a directory-resolved route plus enough information to detect staleness. A future envelope should include:

```text
logical recipient identity
activation generation (optional cached route)
message id
protocol version
correlation / causation ids
deadline
trace context
payload
```

## Invariants

The implementation must maintain these invariants:

1. Two distinct live `GrainId` values never share an `ActivationHandle` within one directory.
2. A handle always reverse-resolves to exactly one logical identity while registered.
3. Dehydration may remove an activation without deleting the logical entity.
4. Rehydration may allocate a new handle without changing logical identity.
5. Persistence/recovery correctness must not depend on reproducing the same activation handle.
6. Distributed migration must not change logical identity.
7. Stale activation routes must be detectable once generations/leases are introduced.

## Why not widen ActorRef immediately?

The current VM stores actor references in a NaN-boxed value and therefore has a 48-bit payload constraint. Replacing the global value representation is a much larger compiler/VM/FFI compatibility project than fixing virtual-actor semantics requires.

Separating logical identity from activation makes the VM representation an optimization detail. Nulang can later widen or indirect `ActorRef` without changing the entity model again.

## Relationship to typed actor protocols

Logical identity and typed protocols are complementary. The eventual type should conceptually be:

```text
EntityRef[Protocol]
  = logical identity
  + protocol identity/version
  + runtime-resolved activation
```

The logical identity is stable; the activation is not. This is the foundation for protocol-aware routing, rolling upgrades, durable message deduplication, and cross-node capability enforcement.

## Follow-up priorities

After Phase 2 runtime integration, the next actor-runtime work should be:

1. typed actor protocols / `ActorRef[P]`;
2. stable message IDs, durable deduplication, and dead letters;
3. actor placement backed by `nulang-capacity`;
4. runtime authority capabilities across distributed boundaries;
5. event/schema evolution and replay compatibility.
