# RFC 0020: Typed Distributed Durable Promises

- **Status:** Draft
- **Tier:** Experimental
- **Author:** Nulang Core Team
- **Created:** 2026-09-15
- **Depends on:** RFC 0005 (durable entities), RFC 0006 (temporal effects), RFC 0007 (event sourcing), RFC 0017 (unified runtime primitives)

## Summary

Introduce typed, single-assignment **durable promises** as the standard way for
one durable computation to suspend until another actor, workflow, external
system or human supplies a value.

A durable promise differs from an in-memory future:

- its identity survives process/node restarts;
- its resolved value is journaled;
- waiting consumes no continuously running thread;
- another actor or an authenticated external caller may resolve it;
- replay observes the recorded result rather than repeating the resolver's
  external action;
- the result type is known statically.

This generalizes today's named `Signal.wait` mechanism without removing it.
Signals remain useful as lightweight named notifications; promises add typed
values, single-assignment semantics and composition.

## Motivation

Long-lived autonomous software frequently needs to suspend on a value that may
arrive much later:

- human approval;
- payment-provider callback;
- child-agent result;
- tool execution on another node;
- quorum of independent reviewers;
- external job completion;
- webhook/API response;
- durable request/reply between entities.

Encoding these cases as polling loops, bespoke signal names or queue plumbing
creates avoidable application-level distributed-systems code.

## Core model

Conceptually:

```nulang
let approval: Promise[Approval] = durable promise()
send reviewer review(order, approval.resolver())
let decision = await approval
```

`Promise[T]` is a durable handle to a result of type `T`.

A promise has exactly one terminal outcome:

```text
Pending
Resolved(T)
Rejected(Error)
Cancelled(reason)
```

Resolution is durable and monotonic. A terminal promise never returns to
`Pending`.

## Identity

Every durable promise has stable identity scoped to a durable entity or
workflow instance:

```text
PromiseId {
  owner_entity_id
  promise_sequence
  generation
}
```

Cloud/external APIs MAY expose an opaque token instead of this internal tuple.
Opaque resolver tokens MUST be unguessable and capability-scoped.

## Resolver capability

A promise handle and the authority to resolve it are distinct.

Conceptually:

```nulang
let p = durable promise[Int]()
let resolver: Resolver[Int] = p.resolver()
```

`Promise[T]` permits awaiting/inspection. `Resolver[T]` permits terminal
resolution according to policy.

This separation lets Nulang's capability system enforce who can complete a
promise and prevents read access from implicitly granting write authority.

Resolver capabilities should be attenuable, for example:

```nulang
let external = resolver.restrict({
  expires: now + 24h,
  principal: payment_webhook,
  attempts: 5
})
```

Exact syntax is non-normative in this RFC.

## Single-assignment and idempotency

The first valid terminal resolution wins.

A repeated resolution request with the same idempotency key and equivalent
terminal value SHOULD succeed idempotently. A conflicting second resolution
MUST fail with a deterministic `AlreadyResolved` result.

The persistence layer therefore records both terminal value and resolver
idempotency identity where available.

## Await semantics

`await promise` behaves like a durable suspension point.

When pending:

1. persist the owner's current durable state/checkpoint;
2. record the promise wait in the durable journal;
3. remove the computation from runnable scheduling;
4. consume no worker thread while idle;
5. reactivate when the promise reaches a terminal state.

When already terminal, `await` completes immediately from persisted state.

Replay MUST return the journaled terminal result and MUST NOT repeat external
resolution effects.

`await` is already reserved in the language and can become the canonical
surface once parser/type/runtime support lands.

## Persistence events

The durable journal should represent at least:

```text
PromiseCreated {
  sequence
  promise_id
  value_type_hash
}

PromiseAwaited {
  sequence
  promise_id
}

PromiseResolved {
  sequence
  promise_id
  value
  resolver_principal?
  idempotency_key?
}

PromiseRejected {
  sequence
  promise_id
  error
  resolver_principal?
  idempotency_key?
}

PromiseCancelled {
  sequence
  promise_id
  reason
  principal?
}
```

The exact serialized format belongs to the persistence-format contract, but
promise state must be reconstructible without re-running resolver code.

## Typed external resolution

Nulang Cloud should permit a promise to be completed externally through an
opaque resolver token or authenticated API endpoint.

Example conceptual flow:

```text
entity creates Promise[PaymentResult]
        |
        +--> external resolver token
                   |
                   v
             payment provider
                   |
               callback
                   |
                   v
          validate PaymentResult
                   |
                   v
           resolve promise once
                   |
                   v
          entity becomes runnable
```

External payloads MUST be validated against the promise's expected schema
before resolution is committed.

## Composition

The standard library should eventually expose durable combinators:

```nulang
await first(a, b, c)
await all([a, b, c])
await race([a, b, c])
await quorum([a, b, c, d, e], 3)
await p timeout 30s
```

Semantics:

- `first`/`race`: resume on the first terminal result according to documented
  success/error policy;
- `all`: resolve only when every member resolves successfully, otherwise obey
  explicit failure policy;
- `quorum`: resolve when `k` acceptable results are available or the quorum is
  mathematically impossible;
- timeout: use the durable timer subsystem, not wall-clock blocking.

Combinator state must itself be durable so crashes do not restart completed
sub-waits.

## Signals

`Signal.wait(name)` remains supported.

Recommended relationship:

```text
Signal                         Promise[T]
------                         ----------
named notification             typed value
may occur repeatedly           single terminal result
simple workflow wakeup         request/reply + composition
Unit-oriented                  typed result/error
```

An implementation MAY lower one-shot typed signals to promises internally in a
future compatibility release, but this RFC does not require that migration.

## Actor ask integration

Long term, durable request/reply can lower to promises:

```nulang
let result = ask remote_actor compute(x)
```

may conceptually become:

```nulang
let reply = durable promise[Result]()
send remote_actor compute(x, reply.resolver())
let result = await reply
```

This avoids tying request/reply lifetime to a network connection or process.
Ephemeral `ask` may remain available as a cheaper local/short-lived primitive.

## Failure and cancellation

Promise rejection/cancellation is data, not process failure by itself.
Awaiting code decides how to handle it.

Owner termination does not automatically erase promise history. Garbage
collection is governed by durable retention policy after no durable reference
or audit requirement needs the promise.

Cancellation authority is separate from resolution authority where security
policy requires it.

## Security

Nulang Cloud resolver operations should record:

```text
promise identity
owner identity
resolver principal/workload identity
code/artifact identity where applicable
capability exercised
input hash
terminal value hash
idempotency key
causal parent event
HLC/timestamp
```

These records should feed the tamper-evident execution provenance chain.

Cross-tenant promise resolution is forbidden unless an explicit capability or
federation policy grants it.

## Type-system requirements

The compiler should eventually enforce:

1. `Promise[T]` resolves only with `T`.
2. `Resolver[T]` cannot resolve a promise of another type.
3. Awaiting a promise returns its typed terminal value/error representation.
4. Resolver capabilities obey normal sendability/authority rules.
5. Durable promise values must satisfy serialization/persistence constraints.
6. A local/ephemeral future cannot silently escape into durable state.

A possible future distinction is:

```text
Future[T]             ephemeral structured concurrency
Promise[T, durable]   persisted distributed completion
```

Do not overload the durable contract onto every local future.

## Operational requirements

Nulang runtime/Cloud observability should expose:

```text
promise id
owner entity
state
created at
waiting since
resolver policy
resolver principal if terminal
dependent waiters
age
```

Operators need to identify promises that are permanently stuck and optionally
resolve/reject/cancel them when policy permits.

## Rollout

1. Define `PromiseId`, terminal state and persistence events.
2. Add runtime store operations with single-assignment/idempotency tests.
3. Add `Promise[T]` / `Resolver[T]` type representations.
4. Implement durable wait/reactivation in the actor/workflow scheduler.
5. Wire reserved `await` syntax to promise suspension.
6. Add external resolver tokens/API in Nulang Cloud.
7. Add `all`, `race`, `first`, timeout and quorum combinators.
8. Optionally lower durable `ask` to promise-based request/reply.
9. Add execution-provenance records for external resolution.

## Non-goals

- Replacing lightweight in-process futures for short-lived parallel work.
- Turning arbitrary mutable shared state into a promise.
- Exactly-once execution of arbitrary external systems; promise resolution is
  single-assignment inside Nulang, while external effects still require their
  own idempotency contracts.
- Polling as the normal implementation strategy.

## Competitive rationale

Durable promises/awakeables are one of the cleanest primitives in modern
durable runtimes because they decouple suspension lifetime from process and
network lifetime. Nulang can improve on the common SDK-level model by making
promise result types, resolver authority, durability and replay semantics
visible to the language and capability/effect systems.