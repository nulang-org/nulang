# RFC 0020: Message Identity and Delivery Invariants

- **Status:** Draft — identity primitives implemented in Phase 1
- **Tier:** Experimental
- **Created:** 2026-09-16

## Summary

Every logical Nulang actor message should have a stable identity that is independent of transport packets, tracing, actor activation ids, and retry attempts.

Phase 1 introduces:

- `MessageId { origin, sequence }`, a canonical 128-bit logical id;
- `MessageIdGenerator`, a lock-free per-origin allocator;
- `MessageMeta`, carrying correlation, causation, retry attempt, and optional deadline;
- the invariant that a retry keeps the same `MessageId`.

Later phases will carry this metadata through local mailboxes, the NUL0 wire protocol, durable journals, deduplication windows, and dead-letter reporting.

## Why packet sequence numbers are insufficient

A transport packet is not the same thing as a logical actor message. One logical message may be serialized more than once because of retry, reconnection, recovery, migration, or relay. Conversely, one transport protocol may introduce packets that are not actor messages at all.

Therefore this is wrong:

```text
packet sequence == message identity
```

The desired model is:

```text
logical MessageId
    ├─ delivery attempt 0 -> packet seq 91
    ├─ delivery attempt 1 -> packet seq 104
    └─ replay after recovery -> packet seq 7 on a new connection
```

All three deliveries represent the same logical operation.

## Identity format

```text
MessageId = origin:u64 || sequence:u64
```

The canonical wire representation is 16 bytes, big-endian.

`origin` is normally the stable node/runtime identity that owns the allocator. `sequence` is monotonically allocated within that origin. Sequence zero is reserved as an unassigned sentinel.

The format intentionally avoids UUID parsing and an additional runtime dependency in the core actor path. It is cheap to compare, hash, serialize, and index in a deduplication table.

## Causal metadata

`MessageMeta` contains:

```text
id
correlation_id
causation_id
attempt
deadline_unix_ms
```

A root message uses its own id as `correlation_id`. A child message receives a new id, retains the parent's correlation id, and records the parent id as its causation id.

This supports causal traces such as:

```text
HTTP request
  -> Order.submit       correlation=A, id=A
      -> Payment.charge correlation=A, id=B, caused_by=A
          -> Audit.log  correlation=A, id=C, caused_by=B
```

Tracing ids remain separate. Trace sampling or exporter changes must never affect delivery identity.

## Retry invariant

A retry is another attempt to deliver the same logical message:

```text
retry.id == original.id
retry.attempt == original.attempt + 1
```

This is the foundation for receiver-side deduplication and effectively-once processing.

Creating a new id on every retry would make duplicates indistinguishable from new work and defeat deduplication.

## Delivery vocabulary

Nulang should use the following precise terms:

- **at-most-once:** no runtime retry; delivery may be lost;
- **at-least-once:** runtime may redeliver the same `MessageId`;
- **effectively-once:** at-least-once delivery plus stable ids and receiver/effect-boundary deduplication;
- **backend-defined:** an external system owns the final guarantee.

Nulang must not claim generic exactly-once execution for arbitrary external effects.

## Planned Phase 2: mailbox and wire integration

Add `MessageMeta` to the runtime mailbox envelope and `Packet::ActorMessage`.

Compatibility requirement: the NUL0 protocol needs an explicit versioned extension path. Older packet readers must not silently reinterpret message ids as payload bytes.

Local sends should allocate an id at the first logical send boundary. Remote serialization must preserve that same id. Forwarding/migration must preserve it as well.

## Planned Phase 3: durable deduplication

Durable actors maintain a bounded deduplication structure keyed by `MessageId`.

On receipt:

```text
if message_id already committed:
    return/replay recorded outcome when applicable
else:
    process
    atomically persist state transition + message_id commit
```

The exact retention policy may be time-, count-, or journal-offset-based. It must be explicit because infinite deduplication history is not operationally viable.

## Planned Phase 4: dead letters

Failed delivery should become observable data rather than silent dropping.

A dead-letter record should include at least:

```text
MessageId
sender
recipient/logical entity identity
behavior/protocol operation
attempt
reason
timestamp
trace context
```

Reasons should distinguish unresolved address, mailbox full, deadline expired, incompatible protocol, unauthorized capability, node unavailable, and retry exhaustion.

## Planned Phase 5: external effects

For replayable external effects, the runtime should expose the stable message/effect id as an idempotency key when the adapter supports it.

Examples include payment APIs, email providers, database commands, and inference requests. This can provide effectively-once behavior only when the external boundary cooperates.

## Non-goals

Phase 1 does not change mailbox layout, NUL0 wire compatibility, persistence formats, or delivery guarantees. It establishes the identity primitive those changes require.
