# Delivery semantics

Nulang distinguishes ordinary remote actor messages from durable workflow/effect execution. A successful local send **enqueues** a packet; it is not proof that an application message was executed remotely.

## Ordinary remote actors

The current NUL0/TCP actor transport is **best-effort / sender-at-most-once**: it does not maintain an outbound unacknowledged application queue and does not retry application messages across a failed connection. A packet may be lost during a partition or connection failure.

- When an actor's sender identity is available, asynchronous **connect** failure is reported to that originating actor as a system-priority delivery-failure message (behavior `0`; payload `[8, 0]`).
- An asynchronous **TCP write** failure is reported as `[8, 1]`. Delivery is **ambiguous**: the peer may already have received all or part of the frame. Do not automatically resend.
- A stopped sender thread reports `[8, 2]`. Synchronous routing and resolution failures use the existing numeric failure-code mapping with a `Nil` second payload slot.
- Failure feedback uses the **system-priority mailbox lane**, not normal-priority admission. It can be delivered even when the sender's bounded normal mailbox is saturated. It is skipped when there is no live originating actor (including sender id `0`).
- A silent network partition need not produce an immediate local connect/write error. Membership liveness, a successful `send`, and address convergence do **not** guarantee remote actor delivery.
- NUL0 wire sequence numbers and ACKs currently support protocol observability and selected higher-level coordination. They **do not** establish retry, deduplication, or at-least-once application-delivery guarantees.

A stronger transport retry contract would require outbound retention **and receiver-side deduplication of the exact message identity before actor dispatch**. Reconnecting and blindly replaying packets after an ambiguous write is unsafe because actor effects could execute twice. Explicit end-to-end application acknowledgements and application-specific idempotency are appropriate when a caller must know the result.

## Durable workflows and effects

Durable workflows and external effects use a separate persistence contract: admitted activation identity, durable transitions, replay-safe receipts, and explicit idempotency where supported. Neither ordinary TCP ACKs nor actor mailbox admission imply exactly-once effects. Failure after an external side effect but before recording its receipt remains ambiguous unless the provider and durable-effect integration enforce the required idempotency contract.

Do not advertise general reliable remote actor delivery until the separate retry/deduplication protocol is implemented and tested under disconnects, partitions, and crashes.

## Implementation boundaries

- `src/runtime/network.rs`: sender-side TCP transport and failure classification.
- `src/runtime/distributed.rs`: routing and actor-visible failure notifications.
- `tests/network_send_failure_feedback.rs`: failure subtype, priority, bounded mailbox, and reconnect regressions.

See issue [#1180](https://github.com/nulang-org/nulang/issues/1180) for the outstanding delivery semantics and recovery requirements.
