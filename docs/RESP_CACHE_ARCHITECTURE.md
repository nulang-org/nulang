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

## Slot migration

Live slot movement is modeled as a two-phase transition in
`CacheSlotMap`. The stable owner remains the source until an explicit
epoch-fenced commit; a transition snapshot carries `source`, `target`, and
the epoch that started the migration. Generic owner reassignment is rejected
for slots with an active transition, so a stale controller cannot bypass the
migration protocol with an ordinary placement update.

In redirect mode the source follows Redis Cluster migration behavior:

- if all command keys are still resident locally, execute on the source;
- if all keys are absent, return `-ASK <slot> <target>`;
- if a same-slot multi-key command mixes resident and absent keys, return
  `TRYAGAIN` rather than splitting execution across owners.

The target interprets the same transition as IMPORTING. It continues to return
`MOVED` under ordinary traffic, but a connection that sends `ASKING`
receives one-command authorization to execute the next request locally. The
authorization lives in `CacheResponsePipeline`, making it connection-local
and one-shot rather than a property of the shard.

Running services install those snapshots through
`CacheServiceHandle::install_placement`. Publication validates endpoint
coverage, rejects non-monotonic epochs, stores the immutable snapshot once
behind a cold-path mutex, then wakes every local reactor. A reactor compares an
atomic published epoch on its Mio wake path and clones a newer snapshot into
its thread-local dispatcher only when needed. Ordinary GET/SET routing never
reads the mutex or shared topology object. Per-shard applied epochs remain
observable so the control plane can detect incomplete convergence before
advancing a migration phase.

### Key transfer and fencing

`CacheStore::export_slot_batch` scans a logical slot in bounded cursor batches
and emits owned key/value payloads, remaining TTL, and an opaque source
slot/generation token. The token is the source-side delete fence: after the
target accepts a value, `finalize_transfer_entry` removes the source copy only
when the key still occupies the same entry slot at the same generation. Any
concurrent SET, INCR, or EXPIRE invalidates the token and forces a reconciliation
pass rather than deleting newer state.

The importing side uses a slot-scoped `CacheTransferImportTracker`. It records
the target entry token created by each accepted source version. Replaying the
same source transfer is idempotent and does not refresh TTL. A newer source
version may replace the prior import only while the target entry is still the
exact version installed by that migration session; if an ASKING-routed client
has mutated or deleted the target key, the old transfer is rejected as a
conflict instead of overwriting client state. Transfers for another logical
slot fail closed.

Relative TTL is carried as remaining milliseconds at export. An importer can
subtract measured transfer elapsed time, and a key whose TTL is exhausted in
transit is not resurrected. This does not claim globally synchronized clocks.
The migration controller can use `live_entries_in_slot` plus stale-finalize
results to decide when another source scan is required and when the source is
fully drained.

Local same-process migration now drives these primitives through bounded,
reactor-owned control queues. `CacheServiceHandle::transfer_local_slot_batch`
requests a source export, submits that exact batch to the target reactor,
finalizes only accepted/expired entries back on the source reactor, and then
queries the source's remaining live-entry count. No `CacheStore` crosses a
thread boundary and the coordinator never takes a store mutex.

Each reactor processes at most a configured control batch per Mio wake, then
re-wakes itself if more control work remains. This prevents a large migration
from monopolizing the shard loop. The coordinator report distinguishes
successful imports, idempotent replays, target conflicts, stale source
versions, bytes moved, cursor progress, and source drain completion. If a
source version raced or the importing target changed independently, the caller
must reconcile rather than committing ownership.

Cross-node migration now carries the same protocol over the existing
authenticated NUL0 transport. It does not add a new NUL0 packet type: cache
traffic is encoded as a bounded binary envelope inside the frozen
`Packet::ActorMessage` shape, using reserved actor id 0 and an internal cache
behavior name. Runtime verifies that the envelope's claimed sender matches the
authenticated transport peer before forwarding it through a bounded bridge to
the cache service.

Every remote command or transfer carries the exact placement epoch plus the
physical source/target owner. The service coordinator rejects stale/future
epochs and owner/migration mismatches before dispatch. The target reactor then
repeats the installed-epoch and owner/migration check immediately before
touching `CacheStore`; this second gate closes the race where topology advances
between coordinator validation and reactor execution.

Remote command responses and transfer acknowledgements are application-level
messages. A NUL0 transport ACK confirms packet processing only and is never
sufficient to finalize migration data. For a remote slot transfer, the source
exports a bounded batch, sends it with a transfer id, waits for the matching
target ACK, and generation-fences source deletion only for entries reported as
Imported, AlreadyImported, or ExpiredInTransit. Conflict, wrong-slot, stale
epoch, mismatched ACK identity, or target-side mutation leaves the source copy
intact and requires reconciliation.

Cross-node monotonic clock origins are intentionally not compared. A transfer
carries the source's remaining TTL, but remote import does not infer wire
transit duration from unrelated process-relative clocks. This preserves clock
correctness but can extend expiry by approximately the network transit/retry
duration. A future transport timestamp based on a synchronized/bounded-error
clock, or an absolute expiry representation with explicit clock assumptions,
can tighten that behavior without weakening the current fencing guarantees.

Remote requests are retry-correlated by authenticated peer plus request or
transfer id. The source-side service keeps at most 4,096 pending operations and
retries them with exponential backoff (10ms initial, 250ms cap, six attempts).
A matching CommandResponse or TransferAck cancels retry only after that event
has been admitted to the application-facing event queue. If retries exhaust, a
command reports an explicit timeout with an **unknown execution outcome**:
the mutation may have committed remotely while every reply was lost. A
transfer timeout is safer: without a matching application TransferAck the
source batch is never finalized, so source data remains authoritative.

The target independently keeps a bounded replay table keyed by authenticated
peer and operation id. Each record stores a BLAKE3 fingerprint of the exact
wire envelope and the original application response/ACK for 120 seconds, well
beyond the automatic retry horizon. An identical duplicate therefore receives
the exact original result without re-running a non-idempotent command or
re-importing a migration batch. Reusing an id with different payload bytes is
rejected. The replay table does not evict unexpired records merely to admit new
work: if all 4,096 protected entries are occupied, new operations fail closed
with TRYAGAIN/conflict until space becomes safe to reuse. This trades bounded
availability under extreme retry pressure for a stronger exactly-once replay
boundary.

Before a live controller publishes a migration commit, it can issue an
exact-epoch `MigrationProbeRequest` to the importing target. The target
revalidates the migration on the owning reactor and returns its current live
slot-entry count, active transfer-import fence count, and cumulative conflict
and wrong-slot observations. The source combines that reply with a fresh
source-reactor slot count. `CacheRemoteMigrationConvergence::ready_for_live_commit`
is true only when the target accepted the exact source/target/epoch tuple, the
source is fully drained, and the target has observed no import conflict or
wrong-slot payload.

The probe is an additional commit gate, not a substitute for transfer ACK
fencing. Source deletion still proves that each removed version had an
application-level import outcome. Conversely, a target conflict observed after
an import is retained in migration-local state and blocks the live commit gate
until the controller reconciles it. Probe request ids use the same bounded
retry/correlation and target replay machinery as remote command and transfer
ids.

The source controller can optionally persist migration proof in
`CacheMigrationJournal`. The journal records the migration's started epoch and
physical source/target identity, then fsyncs the exact TransferBatch envelope
before it may enter NUL0. A matching application TransferAck is fsynced before
any generation-fenced source deletion. Source live-entry counts and convergence
observations are journaled as progress evidence. Recovery can therefore resend
the exact prior batch—with the same transfer id, source generation tokens, TTL
snapshot, and transport epoch—instead of guessing by re-exporting a changed
source.

Journal records use a small versioned binary format with per-record BLAKE3
checksums. Complete checksum failures are corruption and fail closed. A
crash-truncated final record is discarded back to the last valid boundary
before new appends continue. Transfer ids cannot be rebound to different
requests or ACKs, an ACK cannot precede its durable send record, and old
persisted convergence does not authorize commit after controller recovery: a
new probe id and fresh exact-epoch target observation are required.

The ownership commit path itself now consumes this proof. On the source node,
`install_placement` detects a remote migration being committed, requires a
current `CacheRemoteMigrationConvergence` for the exact migration and current
placement epoch, and rejects the publication when that proof is absent or not
ready. With journaling enabled, commit intent is fsynced before publishing the
new owner and completion is fsynced afterward. If publication fails, an abort
record is appended. If the process fails after publication but before the
completion append, replay exposes a pending commit epoch: the outcome is
explicitly ambiguous and must be reconciled rather than assumed.

This makes **controller recovery** durable, but deliberately does not pretend
that an ephemeral CacheStore can survive a cache-service process restart. Each
service build generates a random source-data incarnation and every migration
intent is bound to it. Reopening an old journal from a different incarnation
fails closed. Likewise, a restarted target loses its import-fence state; a
fresh convergence probe then reports fewer fences than the source journal
expects and durable convergence fails. Full cache-process restart recovery
therefore requires a durable CacheStore/target-import-state mode in addition to
this controller journal.

The same cluster layer serves topology discovery without touching CacheStore:
`CLUSTER KEYSLOT` uses the exact router hash, `CLUSTER SHARDS` is the primary
topology response, and legacy `CLUSTER SLOTS` is retained for older clients.
Each current Nulang physical cache owner is advertised as one online master
with a stable 40-hex-character Redis node id derived from its Nulang node/shard
identity. Replicas will be added to these responses when cache replication is
implemented.

When built with the optional `cache-server` feature,
`src/runtime/cache_server.rs` provides a dedicated Mio readiness reactor for
one physical cache shard. The reactor owns the shard's listener, connections,
`CacheStore`, expiration sweep, and ordered RESP pipelines on one thread. It
does not call `Runtime::run_scheduler` and therefore does not inherit the
actor runtime's distributed idle cadence. Cross-shard inbox work wakes the
reactor through Mio's `Waker`; ordinary correctly routed GET/SET requests do
not use that wake path.

The first server surface deliberately requires `Redirect` mode. A connection
that reaches a non-owning shard receives `MOVED` rather than turning the
server into a transparent proxy. A shared `CacheServerClock` gives all shards
in one process the same monotonic millisecond origin for local cross-shard TTL
semantics; process-relative timestamps are not sent to remote nodes. Input,
output, connection count, pipeline depth, inbox drain size, and expiry work are
all bounded by configuration.

At process scope, `CacheServiceBuilder` reserves all local listeners before
constructing the shard dispatchers. This ensures real bound ports, including
ephemeral port allocations, are present in every redirect/topology snapshot.
The builder shares one clock and bounded dispatch-channel set, validates both
stable owners and migration targets, optionally pins each reactor to a logical
CPU, and returns a service handle that shuts down and joins every shard thread
as one unit.

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

1. Add an explicit durable CacheStore mode (WAL/snapshot plus target import-fence
   restoration) before supporting full cache-process restart during migration.
2. Reconcile journaled pending commit intents against durable placement/control
   state after a process restart; never infer the outcome from an empty source.
3. Add a separate transparent proxy endpoint only for non-cluster clients;
   keep the per-shard production listeners redirect-only.
4. Allow topology publication to add/remove advertised remote endpoints without
   restarting local reactors.
5. Promote expiration to a hierarchical timing wheel, then add packed
   aggregate structures and durability acknowledgement modes.
6. Expand RESP compatibility and add Nulang-native leases, locks, semaphores,
   fencing tokens, queues, and stored functions where they fit the product
   boundary.
