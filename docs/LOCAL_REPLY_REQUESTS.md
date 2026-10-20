# Correlated local request/reply tickets (experimental)

This is an additive, typed Rust embedding API built on
[local reply aliases](LOCAL_REPLY_ALIASES.md) from
[#2013](https://github.com/nulang-org/nulang/pull/2013).

It introduces a **one-shot correlated reply ticket** and an explicit
monotonic deadline. No new Nulang syntax, bytecode, protocol frames,
distributed routing, or durable state is introduced.

## API

```rust
use std::time::{Duration, Instant};
use nulang::runtime::{LocalReplyStatus, Runtime};
use nulang::vm::Value;

let ticket = runtime
    .begin_local_reply_request(reply_actor, Instant::now() + Duration::from_secs(5))
    .expect("live local actor and alias capacity available");

// The trusted local responder receives the ticket out of band.
// It chooses an existing reply behavior on reply_actor.
let outcome = runtime.send_local_reply(
    ticket,
    reply_behavior_id,
    &[Value::int(42)],
);
assert_eq!(outcome, LocalReplyStatus::Accepted);

// Each accepted message has arguments:
// [ticket.reference().as_int(), 42].
```

`begin_local_reply_request(owner, deadline) -> Option<LocalReplyTicket>`
requires a live actor and a future monotonic deadline; it returns `None`
for invalid owners, exhausted reference space, alias quota, or deadlines that
already passed.

A `LocalReplyTicket` has two typed fields: an opaque `LocalRequestRef` and
its underlying `ActorAlias`. Call `ticket.reference()` to obtain the request
identity and `ticket.alias()` to obtain the local routing token. The reference
can be encoded as a positive tagged Nulang `Int` by `reference.as_int()`.

`send_local_reply(ticket, behavior_id, values) -> LocalReplyStatus` is one-shot.
It validates the ticket against an outstanding reference and the exact alias;
on accepted mailbox admission it prepends the reference to the reply payload,
then deactivates the alias and retires the request. A second response returns
`Inactive`. **The receiving behavior must have a leading correlation Int
argument**, followed by the sender's supplied values.

Status meanings:

- `Accepted`: locally queued, not executed, durable or acknowledged.
- `Backpressured`: mailbox admission rejected due to capacity; the ticket
  stays active for retry before the deadline.
- `Expired`: the attempt passed the monotonic deadline; the ticket was
  retired and its alias disabled. A subsequent retry returns `Inactive`.
- `Inactive`: unknown/canceled/completed request, forged reference-alias
  pairing, or dead owner.

`cancel_local_reply_request(owner, reference) -> bool` checks exact ownership
and revokes the ticket. It **does not** undo an already admitted reply.
`expire_local_reply_requests(now: Instant) -> usize` is an explicit eager
sweep, not a scheduler timer. Every attempted reply independently checks
`Instant::now()` even if no sweep has run. The sweep returns the number of
expired tickets it canceled. `local_reply_request_count()` exposes the total
number of outstanding local request tickets without exposing their payloads.

Actor reaping invalidates all its outstanding tickets and aliases. Cancel,
timeout, reply admission and actor reaping are serialized by the owning
`&mut Runtime` access. Distinct requests never share an identifier in one
process lifetime; tickets from one runtime cannot route through another.

## Security and operational boundaries

This API is for **trusted in-process callers**. Tickets and aliases are not
unforgeable capabilities, cannot be serialized to an untrusted source, and are
not remote NUL0 addresses. The lower-level `send_to_local_alias` still exists
and can bypass the one-shot protocol when given the alias. Use
`send_local_reply` as the only entry point for correlated responses.

The 4096 aliases-per-actor cap applies to requests too. Applications must
call `cancel_local_reply_request` or `expire_local_reply_requests` for tickets
that complete without a reply. A deadline is checked on delivery, but **expiry
without a new send or explicit sweep does not reclaim the registry entry**.
There is no background timer and no fairness, availability, cross-shard,
cross-node, durable or exactly-once promise.

The request ticket does **not** automatically send an outbound request,
invoke `ask`, resolve a typed `Future`, or wire a monitor DOWN to the
request. Those are higher-level protocol and frontend concerns; adding them
to `Actor` effects requires separate language-level conformance tests and a
stable typed message envelope.

## Exact-head checks

```sh
cargo fmt --all -- --check
cargo test --locked --lib local_request_reply_
cargo test --locked --lib local_reply_alias_
cargo test --locked --lib
cargo test --locked --no-default-features
cargo test --locked --features wasm-backend
```

Focused regressions cover two simultaneous request references, envelope
correlation, duplicate reply suppression, ownership checks, cancellation,
deadline equality, inactive post-expiry aliases, mailbox backpressure and
retry, mismatched ticket fields, eager sweeps, and owner-exit cleanup.
