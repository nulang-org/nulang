# RESP-compatible cache and coordination tier

Status: Experimental implementation plan.

## Product boundary

Nulang's Redis-compatible tier is an edge-native, durable, actor-sharded
coordination and cache service. RESP compatibility is an ingress contract, not
an instruction to represent keys, values, or commands as ordinary Nulang actor
objects.

The data plane follows one rule:

> actors own and coordinate shards; the shard-local cache kernel is invoked
> directly by the owning execution thread.

A local GET/SET must not require a mailbox send, scheduler hand-off, VM heap
allocation, ORCA tracing, or serialization. Those mechanisms remain available
for cross-shard and cross-node coordination.

## Logical and physical sharding

The RESP surface preserves Redis Cluster's 16,384 logical slots and hash-tag
semantics. Logical slots map to a smaller set of physical Nulang cache shards.
The default local placement assigns balanced contiguous ranges to physical
shards; CRC16 already spreads ordinary keys across the logical slot space, so
contiguous ownership keeps cluster topology compact without giving up expected
key balance. Cluster placement may move logical slots between physical owners
without changing the client-visible hash function.

`src/runtime/cache_routing.rs` holds the placement snapshot. Each slot maps
directly to a `(node_id, shard)` owner, so the steady-state routing lookup is
one indexed read after CRC16. Control-plane changes are installed as
monotonically increasing epochs; an entire batch is validated for bounds and
overlap before any slot owner changes. The routing table itself contains no
mutex or mailbox hop.

Multi-key behavior is intentionally tiered:

1. Same-slot operations may be strongly atomic on one shard.
2. Cross-slot reads may scatter/gather but do not imply a global snapshot.
3. Cross-slot write transactions are an explicit Nulang extension and must not
   add coordination cost to ordinary RESP commands.

## Memory model

The first cache kernel in `src/runtime/cache.rs` establishes the representation
boundary:

- small byte strings are inline;
- large keys and values live in reusable size-class arena blocks;
- the key index is a contiguous open-addressed table;
- entry slots are recycled with generations;
- stale expiration records cannot delete a recycled slot.

This is deliberately independent from `Value`, `ActorHeap`, and ORCA. The
production engine should continue in this direction with packed aggregate
encodings for hashes, sets, lists, and sorted sets.

## Expiration

TTL work is separate from actor timers. The initial implementation uses a
hashed timing wheel with generation checks and lazy expiry on reads. The next
iteration should promote this to a hierarchical wheel so long TTLs do not
revisit the same bucket each rotation.

## RESP ingress and command execution

`src/runtime/resp.rs` parses RESP2 array-of-bulk-string commands into borrowed
slices. It validates the complete frame while avoiding a per-command argument
vector. Pipelined frames report their exact consumed length.

`src/runtime/resp_cache.rs` executes the initial compatibility surface directly
against the shard-local kernel: PING, GET, SET (including EX/PX), DEL, EXISTS,
INCR, EXPIRE, TTL, MGET, and MSET. Multi-key commands validate Redis logical
slot equality before execution; cross-slot commands return CROSSSLOT before any
mutation. Same-slot MSET is atomic with respect to other commands because the
owning shard executes one command to completion without yielding.

The network server should retain ownership of the receive buffer until the
local command finishes. `src/runtime/cache_dispatch.rs` classifies the parsed
command before execution: same-shard commands call the cache kernel directly,
other local shards receive one owned frame through a bounded cache-specific
queue, and remote owners produce an explicit transport handoff containing the
slot, node/shard owner, placement epoch, and exact command frame. Queue
saturation is surfaced as backpressure rather than blocking the ingress thread.

Only cross-shard or cross-node commands copy the RESP frame. The same-shard
path remains borrowed and mailbox-free.

RESP pipelining adds a second constraint: replies must remain in request order
even when cross-shard or remote work completes later. `cache_pipeline.rs`
keeps a bounded per-connection response queue. Direct responses flush
immediately when no earlier async request exists; otherwise they wait behind
that request. Local replies are polled from shard reply channels and remote
replies are completed by request id. The sequencer emits only the longest
contiguous completed prefix, preserving RESP ordering without serializing all
commands through one worker.

For Redis Cluster-aware clients, `cache_cluster.rs` supplies preformatted
advertised endpoints keyed by physical shard owner. Dispatch has two explicit
modes:

- `Transparent`: preserve the internal local-shard queue and remote transport
  handoff.
- `Redirect`: when the current endpoint does not own a keyed command's slot,
  return `-MOVED <slot> <host:port>` immediately. The command is not queued or
  proxied.

Redirect mode is the preferred steady-state deployment model for cluster-aware
clients because a warmed client can connect directly to the physical slot
owner. Missing endpoint metadata fails closed instead of silently falling back
to proxying.

The same cluster layer serves topology discovery without touching CacheStore:
`CLUSTER KEYSLOT` uses the exact router hash, `CLUSTER SHARDS` is the primary
topology response, and legacy `CLUSTER SLOTS` is retained for older clients.
Each current Nulang physical cache owner is advertised as one online master
with a stable 40-hex-character Redis node id derived from its Nulang node/shard
identity. Replicas will be added to these responses when cache replication is
implemented.

## Durability

Durability is not implicit in the cache kernel. Add it above the mutation path
as explicit acknowledgement classes:

- memory: acknowledge after local mutation;
- async journal: enqueue WAL append before acknowledgement;
- journal: acknowledge after local durable WAL;
- replica: acknowledge after a configured replica;
- quorum: acknowledge after consensus/quorum.

The default cache path must remain able to run without WAL or consensus work.

## Performance gates

Before calling the service Redis-class, CI benchmarks should track at least:

- local GET hit latency and throughput;
- same-key SET churn for inline and arena values;
- RESP parse cost;
- logical-slot hashing;
- TTL churn and purge cost;
- same-slot MGET/MSET;
- local versus cross-shard command latency;
- p50/p95/p99/p99.9 end-to-end RESP latency under pipelining.

The local hot path target is zero actor messages and zero VM/GC allocations.
Allocator activity in the RESP socket buffer and first-time arena/index growth
must be measured separately from steady-state command execution.

## Next implementation sequence

1. Add ASK/ASKING and migration-state redirects when live slot migration is
   implemented.
2. Build the dedicated cache shard event loop around non-blocking connections,
   inbox draining, ordered response flushing, and per-shard advertised
   endpoints.
3. Connect transparent-mode remote handoffs to a cache-specific cluster
   transport.
4. Wire TCP RESP listeners to shard event loops without a global store lock or
   the actor scheduler's idle cadence.
5. Add hierarchical expiration, packed aggregate structures, and durability
   acknowledgement modes.
6. Add broader RESP compatibility and Nulang-native coordination primitives
   where they fit the product boundary.
