# RFC 0028: Actor Protocol Rolling Upgrades

- **Status:** Draft — compatibility primitives implemented
- **Tier:** Experimental
- **Created:** 2026-09-16
- **Depends on:** RFC 0019 typed actor protocols, RFC 0024 durable schema evolution

## Summary

Nulang should support heterogeneous code versions in one cluster without arbitrary in-place hot-code replacement.

The safe boundary is the actor protocol: each activation advertises one exact protocol revision it currently emits and the exact revisions it can receive. Every revision also carries a schema fingerprint, so two binaries that claim the same version number but encode different message contracts are incompatible rather than silently trusted.

No SemVer behavior is implicit.

## Model

```text
ProtocolSupport {
  protocol: "Counter"
  emits: Revision(v1, fingerprint_a)
  accepts: {
    Revision(v1, fingerprint_a),
    Revision(v2, fingerprint_b)
  }
}
```

The emitted revision and accepted revisions are intentionally separate. That enables a safe two-phase deployment.

## Two-phase rollout

Assume the cluster currently runs v1 and the new binary understands v1 and v2.

### Phase 1 — deploy new code, preserve old wire behavior

```text
old activation:
  emits  v1
  accepts v1

new activation:
  emits  v1
  accepts v1, v2
```

Old and new activations can communicate bidirectionally because both still emit a revision the other side accepts.

### Phase 2 — protocol cutover

After no old activation remains:

```text
new activation:
  emits  v2
  accepts v1, v2
```

A later cleanup release may stop accepting v1 once rollback/compatibility policy permits it.

Switching new actors to emit v2 while old actors still exist fails the bidirectional coexistence check and should block rollout/cutover.

## Fingerprints

Version numbers alone are insufficient. If two artifacts both claim protocol v1 but their canonical message schema differs, they must not communicate as if compatible.

Each `ProtocolRevision` therefore contains:

```text
version: opaque u32
fingerprint: 32 bytes
```

The fingerprint should eventually be derived deterministically from the typed protocol schema produced by RFC 0019, including behavior names, message field/parameter types, reply types, and relevant serialization identity.

## Compatibility rules

For one directional send:

```text
receiver.accepts(sender.emits)
```

For actors that can initiate messages in both directions during a rolling deployment, both directional checks must succeed.

Compatibility is explicit. The runtime does **not** infer any of the following:

- same major version means compatible;
- larger version accepts smaller version;
- additive fields are automatically compatible;
- same version number means same schema.

Those policies can be layered on top later only if Nulang's canonical serialization rules make them provably safe.

## Relationship to durable state

Protocol compatibility and durable-state compatibility are independent gates.

An activation may be message-compatible with another activation but unable to replay its durable history. Conversely, a state migration may succeed while the new code cannot safely communicate with old peers.

A production activation/upgrade gate therefore needs both:

```text
protocol compatibility
AND
event/snapshot migration compatibility (RFC 0024)
```

## Relationship to NUL0

The current NUL0 protocol version is a runtime transport ABI, not an application actor-protocol version.

Actor protocol identity should ultimately be carried inside versioned actor-message metadata. A NUL0 upgrade changes the framing/runtime wire contract; a `Counter` v1 -> v2 change changes an application actor protocol. They must not be conflated.

## Relationship to typed ActorRef

RFC 0019 provides the compile-time protocol surface. This RFC provides its runtime/distributed identity.

The intended eventual lowering is conceptually:

```text
ActorRef[Counter]
    ↓ compiler
Counter canonical schema
    ↓ fingerprint
ProtocolRevision
    ↓ runtime activation metadata
ProtocolSupport { emits, accepts }
    ↓ distributed send
receiver compatibility check
```

## Deployment policy

A future Nulang Cloud/control-plane rollout should:

1. inspect currently active protocol revisions;
2. validate new code's accepted revision set;
3. validate durable schema migration chains;
4. deploy new activations while they still emit the old revision;
5. wait for old activations to drain/dehydrate/terminate;
6. switch emitted revision only when all remaining peers accept it;
7. retain older accepted revisions for the declared rollback window;
8. garbage-collect old protocol support explicitly, never implicitly.

## Failure semantics

A protocol-incompatible message must fail before application behavior execution.

It should become the structured `ProtocolIncompatible` dead-letter reason defined by RFC 0026, carrying message identity/correlation metadata when available.

## Non-goals

This RFC does not provide arbitrary BEAM-style hot code loading, automatic schema conversion, implicit SemVer compatibility, or protocol negotiation by trial-and-error decoding.

## Implemented primitives

`src/protocol_compat.rs` implements:

- exact `ProtocolRevision` identity with 32-byte fingerprint;
- `ProtocolSupport { emits, accepts }`;
- activation protocol registry with duplicate rejection;
- one-way sender→receiver compatibility checks;
- conservative bidirectional coexistence checks;
- validation that an activation can receive the revision it emits;
- tests for staged rollout, premature cutover rejection, fingerprint drift, and absence of implicit version compatibility.

## Follow-up

1. Generate canonical protocol fingerprints from RFC 0019 typed actor signatures.
2. Attach protocol support metadata to compiled actor artifacts and activations.
3. Carry protocol revision in RFC 0026 delivery metadata / future NUL0 ActorMessage v2.
4. Gate placement/activation on both protocol and RFC 0024 persistence compatibility.
5. Expose active protocol generations through actor introspection.
6. Add control-plane orchestration for phase-1 deploy and phase-2 cutover.
