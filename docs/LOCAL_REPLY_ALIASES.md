# Experimental local reply aliases

Nulang's runtime implements an additive Erlang/OTP-inspired **local reply alias**
primitive. This is a Rust host API, **not** a new Nulang-language `Actor` effect,
not a NUL0 wire identifier, and not a distributed reply protocol.

## Contract

```rust
let alias = runtime.create_local_alias(reply_actor_id).expect("live local actor");
assert_eq!(
    runtime.send_to_local_alias(alias, reply_behavior_id, &reply_arguments),
    AliasSendStatus::Accepted
);
assert!(runtime.deactivate_local_alias(reply_actor_id, alias));
assert_eq!(
    runtime.send_to_local_alias(alias, reply_behavior_id, &late_arguments),
    AliasSendStatus::Inactive
);
```

The API has three operations:

- `create_local_alias(owner)`: returns a unique opaque `ActorAlias` while the
  local owner is alive, subject to a cap of 4096 concurrent aliases per actor.
- `send_to_local_alias(alias, behavior_id, args)`: sends only to the live
  local owner. Results: `Accepted`, `Backpressured`, or `Inactive`.
  `Accepted` proves local mailbox admission **not** handler execution,
  durable delivery, or processing success.
- `deactivate_local_alias(owner, alias)`: returns true only if the alias is
  currently active and the provided owner matches. Calls on an inactive alias
  are idempotent in effect and return false.

**Ordering and race boundary:** alias resolution and mailbox admission happen
with mutable access to the *owning Runtime*. Once `deactivate_local_alias`
completes, later sends through that alias return `Inactive`. Messages already
accepted into the actor's mailbox before deactivation remain queued; this is
deliberate and differs from forcibly retracting a reply. Concurrent external
threads must not bypass the owning runtime's single-writer access model.

An owner can hold multiple independent aliases. Deactivating one does not
affect the others. Actor exit/reaping invalidates all aliases of that actor.
Aliases are never reissued in a process lifetime, and tokens created in one
Runtime instance cannot be resolved through another Runtime instance.

## Resource and security boundaries

The registry maintains a forward lookup and a reverse per-owner index, making
actor-exit cleanup proportional to the number of that actor's aliases. Both
maps are runtime-local. The static token generator fails closed at exhaustion;
there is no wraparound. Creating aliases is limited to 4096 active entries per
actor; callers must deactivate aliases as requests complete.

An `ActorAlias` is an opaque Rust value and **not** an authorization token for
a remote or untrusted party. The current API is a trusted host primitive; host
callers can create aliases for arbitrary known local actors. Do not serialize
or expose these identifiers to users, other nodes, or untrusted plugins.

This feature intentionally does **not**:

- provide a source-language effect, `ask` timeout implementation, or
  auto-deactivating single-reply alias;
- cancel an already executing handler or remove previously admitted replies;
- implement cross-shard/cross-node aliases, durable replay, or exactly-once;
- change ordinary `send`/`ask`, monitor references, NUL0 frames, or persistence.

## Validation gates

```sh
cargo fmt --all -- --check
cargo test --locked --lib local_reply_alias_
cargo test --locked --lib
cargo test --locked --no-default-features
cargo test --locked --features wasm-backend
```

The focused regressions cover late sends after deactivation, multiple aliases
per actor, owner-only deactivation, actor-exit cleanup, invalid aliases,
backpressure with retry, per-Runtime isolation, and the active-alias limit.

This is a foundational delivery primitive. A future correlated request/reply
protocol should pair a reply alias with a unique request reference, deadline,
monitor, and explicit terminal state so that cancellation and timeouts have
unambiguous semantics. None of those higher-level guarantees are claimed here.
