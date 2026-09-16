# RFC 0026: Versioned Message Delivery Envelope

- **Status:** Draft — compatibility layer implemented
- **Tier:** Experimental
- **Created:** 2026-09-16

## Summary

Nulang should carry one logical delivery envelope across local mailboxes, remote transport, retries, durable replay, and dead-letter reporting.

The envelope is built around the stable `MessageMeta` introduced by RFC 0020:

```text
DeliveryEnvelope<T>
  meta
    MessageId
    correlation_id
    causation_id
    attempt
    deadline
  sender_actor
  target
  behavior
  priority
  trace_context
  payload: T
```

This RFC adds a versioned binary codec for `MessageMeta` plus structured dead-letter primitives. It intentionally does **not** alter the existing NUL0 `ActorMessage` packet yet. That change requires an explicit wire-version bump and coordinated mailbox/transport adoption.

## Why a separate logical envelope

Transport packet sequence numbers identify physical frames. They do not identify logical work. Retries, recovery, migration, and relay may serialize the same logical message more than once.

Likewise tracing ids are observability identifiers, not delivery identifiers. Sampling or tracing-provider changes must not affect deduplication.

The logical envelope therefore owns message identity and causality while concrete mailbox, persistence, and wire representations adapt it as needed.

## Metadata wire format v1

The compatibility layer defines a fixed 66-byte representation:

```text
0..4    magic `NDM1`
4       metadata version = 1
5       flags
6..22   message id
22..38  correlation id
38..54  causation id slot
54..58  delivery attempt
58..66  deadline slot
```

Flags indicate whether causation and deadline slots are active. Unknown versions or flag bits fail closed.

The fixed-size representation is deliberately independent of NUL0 framing so a future transport revision can include it unambiguously.

## Dead letters

Current distributed delivery reports some failures back to the sender using integer codes and the runtime has a `dlq_actor_id` placeholder. Those mechanisms do not retain logical message identity, target, behavior, attempt, timestamp, or causal context.

A structured dead letter contains:

```text
MessageMeta
sender actor
logical target
behavior
machine-readable failure reason
failure timestamp
trace context
```

The initial bounded queue is an exact FIFO. Capacity zero disables retention. Future runtime integration may additionally forward records to the existing system DLQ actor or persistence/telemetry sinks.

## Failure reasons

The initial vocabulary includes:

- unresolvable target
- node unavailable
- target actor missing
- mailbox full
- deadline expired
- protocol incompatible
- capability denied
- payload encoding/materialization failure
- behavior unavailable
- retry exhausted
- spawn rejected
- explicit fallback detail

Runtime-specific failures should map into this vocabulary instead of inventing unstable integer codes at each call site.

## Retry and deadline semantics

Retrying preserves `MessageId`, correlation, causation, target, behavior, and payload. It increments only the attempt count.

A delivery with an absolute deadline is expired when:

```text
now_unix_ms >= deadline_unix_ms
```

Expired work should become a dead letter rather than entering an application mailbox.

## NUL0 migration plan

The existing `Packet::ActorMessage` layout must not be modified under the current wire version. Integration should proceed as follows:

1. Adopt `DeliveryEnvelope` at local send boundaries.
2. Add metadata to mailbox messages while preserving legacy constructors during migration.
3. Bump `WIRE_VERSION` and extend `ActorMessage` with the fixed metadata block.
4. Preserve the same metadata when serializing retries, queued remote-spawn messages, behavior-fetch retries, and migration handoff.
5. Decode metadata before mailbox insertion and reject incompatible metadata versions explicitly.
6. Feed successful durable commits into the bounded dedup window from RFC 0020.
7. Replace integer delivery-failure notifications with structured dead-letter production while keeping a compatibility adapter for older actor code.

Mixed wire versions remain rejected by the existing handshake. Nulang should not add heuristic packet parsing to support old and new layouts on one version number.

## Delivery guarantees

This RFC does not claim generic exactly-once execution.

The intended model remains:

- transient/local sends: implementation-defined at-most/at-least-once behavior;
- remote durable sends: stable identity plus retries;
- effectively-once processing: stable ids + receiver dedup + atomic durable commit where supported;
- external effects: backend-defined and dependent on idempotency support at the external boundary.

## Non-goals

This phase does not alter NUL0, mailbox structs, persistence formats, scheduler behavior, or retry policy. It establishes the compatibility-safe envelope and diagnostic model those integrations require.
