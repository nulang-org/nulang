# Nulang Fabric

Nulang Fabric is the runtime substrate for messaging, distributed state, streams,
coordination, and durable actor communication. It is intentionally built on top
of Nulang's existing actor runtime, NUL0 transport, cluster membership, CRDTs,
and persistence rather than as a second broker embedded beside them.

## Current implementation

The first implementation slice is ephemeral, shard-local topic routing.

Runtime APIs:

- `Runtime::fabric_subscribe(pattern, actor_id, behavior)` — fan-out subscription.
- `Runtime::fabric_subscribe_group(pattern, group, actor_id, behavior)` — competing
  consumer subscription.
- `Runtime::fabric_unsubscribe_actor(actor_id)` — remove an actor's subscriptions.
- `Runtime::fabric_subscription_count()` — shard-local subscription gauge.
- `Runtime::fabric_publish(topic, args)` — publish to matching subscribers.

Subject patterns use NATS-style token matching:

- `orders.created` matches exactly that topic.
- `orders.*` matches one token after `orders`.
- `orders.>` matches one-or-more tokens after `orders` and `>` must be terminal.

Non-group subscriptions fan out to every matching actor. For each matching
consumer group, Fabric selects one member using deterministic round-robin
routing. Identical subscriptions are deduplicated.

Subscriptions are lifecycle-bound to their actors: normal exit, faults, linked
exit cascades, and supervisor shutdown remove the actor's ephemeral routing
entries. Subscription registration also verifies that the requested behavior
exists. Publication resolves the behavior again and uses numeric delivery,
explicitly avoiding the legacy actor-send fallback where an unknown behavior
name can map to behavior 0.

This layer has deliberately **no durability guarantee yet**. It defines routing
semantics that later cluster and stream layers can reuse.

## Design constraints

1. **Do not create a parallel broker runtime.** Fabric should extend the existing
   actor/distribution substrate.
2. **Preserve locality.** Same-shard delivery should remain ordinary actor mailbox
   delivery; cross-shard and cross-node routing should only pay their respective
   transport costs.
3. **Keep ephemeral and durable semantics distinct.** Topics should not require a
   replicated log. Streams do.
4. **Use consensus only for state that requires strong ordering.** CRDT-backed
   convergent state should remain available for workloads that do not need
   linearizability.
5. **Make deterministic testing a first-class constraint.** Routing order and
   failure behavior must remain reproducible under the existing DST harness.
6. **Prefer typed language primitives over stringly broker APIs.** String subjects
   are a runtime/interoperability layer; future `topic` and `stream` declarations
   should provide compile-time payload types and capabilities.
7. **Do not hide shard coordination in process-global state.** Cross-shard Fabric
   control messages should extend the existing shard transport explicitly so
   independent runtimes and deterministic tests remain isolated.

## Roadmap

### Phase 1 — ephemeral messaging

- [x] Subject validation and wildcard matching.
- [x] Fan-out subscriptions.
- [x] Consumer groups.
- [x] Duplicate suppression.
- [x] Explicit actor cleanup.
- [x] Automatic cleanup during actor exit.
- [x] Behavior existence validation and safe numeric delivery.
- [ ] Cross-shard subscription routing.
- [ ] Mailbox capacity and explicit backpressure policies.

### Phase 2 — cluster-wide topics

- [ ] Subscription advertisement through cluster membership/distribution.
- [ ] Remote topic delivery over NUL0.
- [ ] Node-loss cleanup and subscription convergence.
- [ ] Placement-aware consumer selection using mailbox pressure and locality.
- [ ] Deterministic multi-node simulation coverage.

### Phase 3 — durable streams

- [ ] Append-only segmented log.
- [ ] Partition ownership and replication.
- [ ] Durable consumer cursors.
- [ ] ACK/NACK and redelivery.
- [ ] Replay and seek by sequence/time.
- [ ] Retention policies.
- [ ] Dead-letter streams.
- [ ] Deduplication/idempotency keys.

### Phase 4 — distributed state and coordination

- [ ] Distributed maps and expiring cache entries.
- [ ] Atomic counters and compare-and-set.
- [ ] Leases and leader election for strong coordination.
- [ ] Native rate limiters and semaphores.
- [ ] CRDT-backed eventually consistent maps/sets/counters.

### Phase 5 — language integration

Target source-level direction (illustrative, not yet syntax-stable):

```nulang
topic OrderEvents: OrderEvent

stream Payments: PaymentEvent {
  retention 7d
  replicas 3
}
```

The compiler should eventually carry payload types, required capabilities,
serialization metadata, and compatibility information into Fabric routing.

## Compatibility strategy

Native Fabric semantics should stabilize before compatibility gateways are
added. The likely order is:

1. NATS core protocol subset for `PUB`/`SUB`-style interoperability.
2. JetStream-like semantics backed by Fabric streams where mappings are exact.
3. RESP subset for common Redis cache/state operations.

Compatibility is a migration surface, not the architecture. Nulang programs
should use actors, typed topics, streams, and distributed state directly.
