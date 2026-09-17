# Nulang Fabric

Nulang Fabric is the runtime substrate for messaging, distributed state, streams,
coordination, and durable actor communication. It is intentionally built on top
of Nulang's existing actor runtime, NUL0 transport, cluster membership, CRDTs,
and persistence rather than as a second broker embedded beside them.

## Current implementation

The current implementation provides ephemeral topic routing across a
Fabric-enabled Runtime shard set and the node-aware directory primitives needed
for cluster-wide topics.

Runtime APIs:

- `Runtime::new_fabric_sharded(num_shards)` — create normal runtime shards plus
  Fabric's subscription-metadata control plane.
- `Runtime::wire_fabric_shards(shards)` — attach Fabric metadata replication to
  an existing ordered shard set before subscriptions are registered.
- `Runtime::fabric_sync()` — apply pending cross-shard subscription metadata.
- `Runtime::fabric_subscribe(pattern, actor_id, behavior)` — fan-out subscription.
- `Runtime::fabric_subscribe_group(pattern, group, actor_id, behavior)` — competing
  consumer subscription.
- `Runtime::fabric_unsubscribe_actor(actor_id)` — remove a local actor's
  subscriptions without touching numerically-colliding actors on remote nodes.
- `Runtime::fabric_advertisements(limit)` — export a bounded snapshot of this
  node's local subscriptions for the cluster control plane.
- `Runtime::fabric_replace_remote_advertisements(node_id, advertisements)` —
  atomically replace the known remote subscription snapshot for one node.
- `Runtime::fabric_remove_remote_node(node_id)` — purge routes learned from a
  failed or removed cluster node.
- `Runtime::fabric_subscription_count()` — subscription gauge for the local
  routing view.
- `Runtime::fabric_remote_subscription_count()` — remote-subscription gauge.
- `Runtime::fabric_publish(topic, args)` — publish to matching local, cross-shard,
  or already-known remote subscribers.

Subject patterns use NATS-style token matching:

- `orders.created` matches exactly that topic.
- `orders.*` matches one token after `orders`.
- `orders.>` matches one-or-more tokens after `orders` and `>` must be terminal.

Non-group subscriptions fan out to every matching actor. For each matching
consumer group, Fabric selects one member using deterministic round-robin
routing. Queue-group cursors are scoped by concrete topic plus group name, so
unrelated subjects using the same group label cannot perturb each other's
selection order. Identical subscriptions are deduplicated.

Subscriptions are lifecycle-bound to their actors: normal exit, faults, linked
exit cascades, and supervisor shutdown remove the actor's ephemeral routing
entries. Subscription registration verifies that the requested behavior exists
and captures its numeric behavior id. That metadata is replicated to the other
Fabric-enabled shards. A publisher therefore selects targets once from its
local routing view and then calls the normal `send_message_by_id` path; payloads
use the existing local/cross-shard actor transport rather than a second message
transport.

Fabric now distinguishes local and remote routing identities explicitly. Local
subscriptions retain numeric behavior ids for fast same-process delivery.
Remote subscriptions retain `(node_id, actor_id, behavior_name)` because numeric
behavior-table ids are node-local. Remote publication therefore reuses
`Runtime::send_distributed` and the ordinary distributed actor message path.
Actor ids are not assumed to be globally unique: removing local actor `42`
never removes a remote node's actor `42`.

The cluster-directory API uses complete per-node snapshots. Replacing a node's
snapshot removes stale routes for that node before inserting the new set, and
node-loss cleanup can delete the node's routing state immediately. The remaining
Phase 2 wire step is to carry these snapshots as a backward-compatible additive
tail on existing cluster gossip; Fabric does not need a new payload transport or
new broker connection.

The Fabric control plane is explicit and runtime-owned. It uses small in-process
channels only for subscription lifecycle metadata. There is no process-global
subscription registry, so separate Runtime groups and deterministic tests remain
isolated.

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
7. **Do not hide shard coordination in process-global state.** Fabric shard
   metadata channels are owned by the Runtime shard set and payload delivery
   continues to use the existing actor transport.
8. **Do not fork the NUL0 payload path.** Cluster-wide Fabric metadata may extend
   the existing compatible gossip envelope, while actual remote publications
   remain ordinary distributed actor messages.
9. **Scope lifecycle by node identity.** Bare actor ids are insufficient for
   remote subscription deletion or failure cleanup.

## Roadmap

### Phase 1 — ephemeral messaging

- [x] Subject validation and wildcard matching.
- [x] Fan-out subscriptions.
- [x] Consumer groups.
- [x] Duplicate suppression.
- [x] Explicit actor cleanup.
- [x] Automatic cleanup during actor exit.
- [x] Behavior existence validation and safe numeric delivery.
- [x] Cross-shard subscription replication and payload routing.
- [x] Queue-group cursors scoped by topic and group.
- [ ] Publish/backpressure result accounting on top of bounded actor mailboxes.
- [ ] Placement-aware queue-group selection across publishing shards.

### Phase 2 — cluster-wide topics

- [x] Node-aware local/remote subscription representation.
- [x] Per-node advertisement snapshot export/replacement APIs.
- [x] Node-scoped remote route cleanup primitive.
- [x] Remote routing path reuses `Runtime::send_distributed`.
- [ ] Carry subscription snapshots as an additive NUL0 gossip tail.
- [ ] Invoke node cleanup from cluster failure/removal handling.
- [ ] Multi-node subscription convergence.
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
