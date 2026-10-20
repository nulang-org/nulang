# Experimental correlated replies from Nulang source

This feature is stacked on the typed local runtime reply tickets in
[#2015](https://github.com/nulang-org/nulang/pull/2015) and local aliases in
[#2013](https://github.com/nulang-org/nulang/pull/2013). It is not a distributed
request protocol or a replacement for synchronous `ask`.

## Source-level API

```nulang
actor Receiver {
    behavior new_request(responder: Actor) {
        perform Actor.reply_open(responder, 5000)
    }

    behavior reply(reference: Int, value: Int) {
        // Only the designated responder can send this reply effect.
        // Still validate correlation against local pending request state.
        perform IO.print(perform Int.to_string(value))
        0
    }

    behavior cancel_request(reference: Int) {
        perform Actor.reply_cancel(reference)
    }
}

actor Responder {
    behavior answer(reference: Int) {
        perform Actor.reply_send(reference, 42)
    }
}
```

- `Actor.reply_open(responder: Actor, ttl_ms: Int) -> Int | Nil` creates a
  local one-shot reply request owned by the current actor, **bound to exactly
  one designated live responder actor**. The timeout must be positive and no
  longer than 24 hours. It returns the positive correlation reference or nil
  for a missing actor, invalid responder, allocation failure, or invalid TTL.
- `Actor.reply_send(reference: Int, value: Int) -> Int | Nil` attempts to
  deliver to the **named `reply(reference: Int, value: Int)` behavior on the
  ticket owner**. Only the `responder` specified by the owner at
  `reply_open` may send; other actors receive inactive (3), whether or not
  the reference exists. Its return code is `0` accepted, `1` backpressured,
  `2` expired, `3` inactive. Nil denotes malformed input or a missing
  reply behavior. Backpressure leaves the request active for retry.
- `Actor.reply_cancel(reference: Int) -> Bool` revokes a request belonging
  to the current actor. False means invalid, not owned by this actor, or
  already retired. Canceled requests reject future sends.

The runtime prepends the correlation `Int` to the reply payload. Accepted
means **mailbox admitted**, not processed or acknowledged. Successful replies
are one-shot and invalidate the underlying alias. Deactivation does not
retract a previously accepted message. A reply handler should check the
correlation reference against its own expected state before applying results.

These experimental effects are for **trusted, local actor code only**.
References are positive tagged `Int` values and are **not unforgeable
capabilities**: sender identity comes from the runtime actor context, while
the integer reference is only a correlation key. A different local actor
cannot claim the designated responder role by guessing the reference.
However, these effects do not authenticate external origins or authorize
access across tenant boundaries. Do not use them for untrusted tenant security boundaries,
credentials, billing authorization, cross-shard routing, or remote nodes.
Host-level `LocalReplyTicket` and `ActorAlias` are opaque Rust handles but
still not independently authenticated over the network. Trusted native
host callers can bypass this source-level sender binding with the lower-level
Rust alias APIs; those APIs must not be exposed to untrusted code.

The source-level API has no timer wakeup or automatic garbage collection on
deadline alone. Reply attempts check time, while `expire_local_reply_requests`
is the explicit host-side sweep. More complete `await reply` semantics
require durable timer integration, cancellation-safe receiver handling,
per-request typed protocol definitions, and cross-node routing.

## Correlation-aware selective receive (Rust mailbox primitive)

`Mailbox::receive_match_ref(behavior_id, reference)` performs indexed lookup
by `(behavior_id, first_argument_as_Int)` across System, Local, and Normal
mailbox lanes. It uses the same transactional `commit_receive_match()` and
`reset_receive_match()` rules as `receive_match`, preserving unmatched
messages and guard rejection semantics. Indexes are allocated lazily when
selective receive is first used and rebuilt after removals. The new
correlation index can increase selective-receive metadata memory for actors
with many distinct integer-first messages.

This primitive is **not yet automatically selected by the Nulang compiler's
general `receive` syntax**. Source handlers receive the correlation value as
an ordinary typed parameter. A compiler optimization can lower proven
literal-reference receive patterns to this path in a separate PR without
changing general-case behavior.

## Focused validation

```sh
cargo fmt --all -- --check
cargo test --locked --lib actor_reply_effect_
cargo test --locked --lib correlated_receive_
cargo test --locked --lib local_request_reply_
cargo test --locked --lib
python3 conformance/run.py
cargo test --locked --no-default-features
cargo test --locked --features wasm-backend
```

The new conformance fixture is
`conformance/behavior/actor_19_correlated_reply_effects.nula`; its expected
output tests unauthorized spoofing (3), accepted (0), duplicate/inactive
(3), canceled and late (3) source-level reply behavior. Rust tests cover the
reply payload correlation, designated responder checks, owner-checked
cancellation, invalid timeouts and missing handlers, indexed
out-of-order selection, reset, arrival, and priority lanes.

No compile/test result is claimed until the exact-head CI runs.
