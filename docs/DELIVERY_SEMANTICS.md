# Delivery semantics

## Ordinary remote actors

Ordinary remote actor sends are **best-effort, at-most-once**. The transport does not retain unacknowledged application packets and does not retry them across reconnects.

- A connect failure must be surfaced to the originating actor as a delivery-failure system message; the notification uses the system-priority mailbox lane so normal mailbox backpressure cannot hide it.
- A TCP write failure must also be surfaced, but is treated as **ambiguous**: some bytes may already have reached the peer, so the runtime must not blindly resend the packet.
- The existing wire sequence number and `Ack` packet are protocol/observability primitives only. They do not currently imply an at-least-once retry contract.
- A silent network partition can remain indistinguishable from delayed traffic until higher-level failure detection acts; the transport cannot synchronously prove non-delivery for a packet dropped outside the process.

Any future transport-level retry policy must retain outbound packets **and** deduplicate receiver-side sequence identities before actor delivery. Blind reconnect-and-resend after a write failure is forbidden because it can duplicate actor effects.

## Durable workflows and effects

Durable workflow/effect guarantees are a separate layer. Durable execution may use persisted intent, idempotency, replay, and atomic transition machinery to provide stronger semantics than ordinary actor sends. Those guarantees must not be inferred from the ordinary TCP actor transport.
