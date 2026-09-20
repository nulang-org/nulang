# RFC 0024: Authoritative Logical Actor Ownership Records

- **Status:** Draft — ownership state machine + fenced in-memory persistence reference implemented
- **Tier:** Experimental
- **Created:** 2026-09-20
- **Depends on:** RFC 0022 (logical actor identity and activation fencing)

## Summary

Nulang Cloud needs one authoritative ownership record for every active logical actor.

The record is keyed by the full `GrainId`, carries the compact
`LogicalActorId` for wire/storage indexing, and records:

- owner node;
- runtime-local activation handle;
- authoritative activation epoch.

The runtime state machine implemented in
`src/runtime/logical_actor_directory.rs` defines valid ownership transitions.
`src/runtime/logical_actor_persistence.rs` adds a reference in-memory store
keyed by full `GrainId` that rejects durable writes from stale nodes/epochs.
It deliberately does **not** implement consensus. A control plane must persist
and serialize these transitions using a linearizable mechanism before runtimes
treat them as authoritative.

## Core invariant

For one full logical actor identity:

```text
at most one live owner may exist at one authoritative epoch
```

A higher epoch fences every older owner. Equal-epoch claims from different
nodes are not resolved by arrival order; they are reported as an explicit
conflict.

## Why the full GrainId is the key

`LogicalActorId` is a compact 128-bit digest and is useful for protocol,
indexing, tracing, and storage layouts, but it is not the final collision
authority.

The in-memory authoritative directory therefore keys records by full
`GrainId`. Future compact indexes must retain enough information to detect a
digest collision rather than silently aliasing two actors.

## Ownership transitions

### Grant

`grant(grain, node, handle, epoch)` obeys:

1. an epoch older than the highest observed epoch is rejected;
2. replaying the exact same grant is idempotent;
3. a different owner at the same epoch is an explicit conflict;
4. a grant for an epoch already fenced/released is rejected;
5. a strictly newer epoch replaces prior ownership.

### Fence

`fence(grain, epoch)` removes live ownership while retaining the epoch as a
tombstone.

A fence may advance directly to a newer epoch even when no replacement node is
ready. This allows the control plane to invalidate all older owners during
failure handling before selecting a new placement.

Once an epoch is fenced, ownership cannot be resurrected at that same epoch.

## Routing fence vs durable commit fence

Routing and durable storage need related but different predicates.

A **route** is current only when all four values match:

```text
GrainId + node + ActivationHandle + ActivationEpoch
```

The handle belongs here because it distinguishes local activation
incarnations.

A **durable commit** is authorized by:

```text
GrainId + node + ActivationEpoch
```

The activation handle is intentionally excluded from the durable key. Handles
are local ephemeral routing metadata and may change after reactivation,
migration, or process restart.

This distinction prevents the VM's current 48-bit ActorRef representation from
leaking into long-lived storage correctness.

## Equal-epoch conflicts

Two nodes presenting different owners for the same logical actor and epoch is
a split-brain condition.

The runtime directory returns `EqualEpochConflict`; it never chooses a winner
from timing, node id, lexical order, or "last write wins".

The ownership service must resolve the conflict and publish a newer
authoritative epoch.

## Control-plane responsibility

A production Nulang Cloud implementation should place ownership transitions
behind a linearizable authority such as:

- a Raft-backed metadata service;
- a transactional database row using compare-and-swap / serializable
  transactions;
- another consensus-backed lease/epoch allocator.

The exact mechanism is deliberately outside the runtime state machine. What
matters to runtimes is receiving an authoritative monotonic epoch and ownership
grant.

## Required persistence model

The future durable record should contain at least:

```text
canonical logical identity
logical actor digest/version
owner node
activation epoch
optional local activation handle
record version / compare-and-swap revision
```

The record must survive node loss. Keeping it only in gossip or process memory
does not provide fencing.

## Required write path

Durable actor state commits should eventually carry the activation epoch.

Before accepting a commit, storage must check:

```text
submitted node == authoritative node
submitted epoch == authoritative epoch
```

A stale runtime can then continue executing briefly after partition or GC pause
without being able to corrupt authoritative state.

## Relationship to RFC 0022

RFC 0022 separates logical identity, local activation handles, and activation
epochs and defines the local external-authority handoff.

This RFC supplies the deterministic distributed ownership record that a future
control plane can persist and use to drive that handoff.

The intended flow is:

```text
control plane allocates epoch / owner
              |
              v
LogicalActorOwnershipDirectory
              |
              v
ActivationDirectory::observe_authoritative_epoch
              |
              v
ActivationDirectory::install_authoritative_activation
              |
              v
local execution + epoch-fenced durable commits
```

## Follow-up work

1. Persist ownership records in Nulang Cloud with compare-and-swap semantics.
2. Add ownership epoch/node to virtual-actor routing envelopes.
3. Adapt production persistence backends to the logical-identity keyed fenced
   commit contract demonstrated by `FencedLogicalActorStore`.
4. Feed node failure/migration decisions through authoritative ownership
   transitions instead of directly respawning by actor number.
5. Add deterministic partition tests proving stale owners cannot commit after a
   replacement epoch becomes authoritative.

## Non-goals in this phase

- no Raft implementation;
- no lease timing policy;
- no quorum selection;
- no gossip-based winner election;
- no persistence-format migration;
- no change to the NaN-boxed ActorRef ABI.
