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
hashed timing wheel with generation checks and lazy expiry on reads. Each explicit sweep inspects at most its candidate budget, including stale and
future records, and keeps partially drained buckets eligible for later sweeps.
FIFO records and round-robin bucket selection prevent a future collision or a
large expiration burst from monopolizing subsequent sweeps. Bucket discovery
is bounded by the fixed wheel size. Slot-indexed intrusive FIFO handles unlink
the previous expiration record in O(1) on TTL refresh, persistent overwrite,
explicit deletion, and lazy expiration. Each TTL-bearing entry retains at most
one linked record; a reusable node table retains capacity at the highest slot
index that has carried a TTL. Generation and deadline checks remain defensive
guards. This is not a wall-clock CPU deadline or a total memory admission cap.
The next iteration should promote the wheel to a hierarchy so long TTLs do not
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

1. Add a multi-shard server builder that reserves/binds advertised endpoints,
   shares one `CacheServerClock`, pins reactor threads when requested, and
   starts/stops the shard set as one service.
2. Add ASK/ASKING and migration-state redirects when live slot migration is
   implemented.
3. Add a separate transparent proxy endpoint only for non-cluster clients;
   keep the per-shard production listeners redirect-only.
4. Connect remote transparent handoffs to a cache-specific cluster transport.
5. Promote expiration to a hierarchical timing wheel, then add packed
   aggregate structures and durability acknowledgement modes.
6. Expand RESP compatibility and add Nulang-native leases, locks, semaphores,
   fencing tokens, queues, and stored functions where they fit the product
   boundary.


## Cache admission limits

The default store admits at most 1,000,000 physical entries and 256 MiB of
initialized size-class arena extent. Use CacheStore::with_limits(CacheLimits
{ max_entries, max_arena_bytes }) to configure a shard before binding its
server. SET, INCR creation, and MSET reject over-budget requests with RESP OOM;
native set_bytes and set_integer return Result. MSET preflights the entire
batch with distinct-key counting and conservative free-block accounting before
mutation, so a rejected batch does not partially commit. Blocks released by a
write are not credited until subsequent admission; replacement can therefore
be rejected even when a more sophisticated allocator could accommodate it.
Expired entries still physically present count toward the limit.

Memory stats distinguish initialized arena extent from estimated retained
capacity (including key index, slots, free lists, TTL metadata and sweep
scratch). The latter is diagnostic, not a hard cap: allocator overhead,
capacity rounding, temporary command buffers and connection/queue memory are
not included in the enforced arena budget. Aggregate server budgets, eviction,
fallible allocator handling and workload-based limit tuning remain separate
work.


## Shared reactor connection buffer budget

The redirect-only cache reactor reserves input/output vector capacity before
growth, with a default 256 MiB aggregate connection budget per reactor.
Embedders can pass the same CacheBufferBudget to CacheShardServer::bind_with_budget
on multiple reactors for a shared process-level connection-buffer allowance.
The default bind method creates its own budget. Capacity stays charged while retained. After each successful reactor drive,
fully drained input and output vectors release their allocations and charges
even when the connection remains open. Incomplete frames and unsent replies
retain capacity until they drain; RAII releases remaining charges on
disconnect or reactor teardown. Admission or allocation failure closes the offending
connection rather than letting buffers grow without limit.

Before executing a complete command, the reactor conservatively sizes its
reply using read-only cache lookups, the same routing snapshot and clock,
and topology/redirect response bounds. This prevents repeated-key MGET and
other large responses from allocating beyond the configured output limit.
The redirect-only reactor writes synchronous replies in order directly to
the reserved output vector, without the asynchronous pipeline scratch copy.
The standalone asynchronous pipeline remains unchanged.

This is a connection-vector capacity budget, not an RSS guarantee. Allocator
overhead and transient reallocation, OS socket buffers, connection metadata,
topology and MSET preflight scratch, cache store memory, and asynchronous
cross-shard queues are separate. Conservative reply bounds may reject a
response that would fit if encoded exactly; the policy is close-on-pressure
rather than waiting or evicting another connection. Immediate reclamation of
empty vectors trades allocation reuse for predictable idle memory availability;
benchmark the effect on short request/response workloads. Fair scheduling,
partial-frame deadlines and full-system memory qualification remain future
work.
## Reference-server verification

The explicit same-slot RESP2 differential example starts a fresh local Redis or Valkey
process on a private Unix socket (TCP and persistence disabled), compares raw
replies against the shard-local command executor, and cleans up the process.
Run each installed reference implementation explicitly:

```sh
NULANG_RESP_ORACLE=redis-server cargo run --no-default-features --example resp_cache_oracle
NULANG_RESP_ORACLE=valkey-server cargo run --no-default-features --example resp_cache_oracle
```

The suite covers the supported ten commands, binary payloads, duplicate keys,
canonical integers and overflow, TTL rounding, and deterministic seeded
mutation sequences. The seed and step are included in mismatch diagnostics.
A dependency-free regression test runs even without an external server.
Cross-slot rejection remains a separate Nulang cluster-contract test, since
standalone reference servers permit cross-slot commands. This is a kernel
compatibility gate, not a claim of complete Redis command, network, persistence,
or cluster compatibility. Verify executable versions independently; some
distributions install Valkey under both binary names.


## Write-ahead journal substrate (Unix)

CacheWal is the framing and recovery primitive used by the native
DurableCache coordinator, now also used by the optional durable RESP reactor. It uses a distinct eight-byte NLCWAL02 format
identifier, complemented u32 payload length, contiguous u64 sequence, and
BLAKE3 checksum of the entire record header and payload. Each record contains
one opaque logical mutation or batch; callers must validate and encode that
payload and apply it atomically. Earlier experimental NLCWAL01 files are
not accepted. The existing BLAKE3 dependency is reused.

Defaults limit one record to 8 MiB and the journal to 1 GiB, configurable
with WalLimits. Opening takes an exclusive advisory file lock, syncs the
directory entry, and streams one record at a time through a borrowed
payload callback. The caller must recover into private staging state and
publish only after successful open; callback side effects cannot be rolled
back. A physically incomplete final record is truncated and synced. A
bad complete header, checksum, sequence, or file size fails closed without
truncation. An incomplete file header is rejected rather than reinitialized.

Buffered append acknowledges OS writes; Data append additionally syncs file
data before acknowledging. An I/O error or unwind during append poisons
the handle and prohibits further appends until a fresh recovery. A failed
sync is ambiguous: a complete unacknowledged record may replay after reopen.
Record/file admission failures happen before I/O and do not poison the handle.

Use a trusted private journal directory; advisory locks do not protect against
non-cooperating writers or external file replacement. The journal itself does not interpret mutations. The native coordinator below
supplies mutation encoding, TTL translation and a write-before-publication
boundary. Replication and physical power-loss qualification remain pending.


## Native durable mutation coordinator (Unix)

DurableCache owns the CacheStore and CacheWal exclusively, without a mutable
store escape hatch. Each accepted mutation is encoded, preflighted against
cache admission limits, appended using the configured acknowledgement mode,
then applied to the live store. Native set_bytes, set_batch, delete_many,
increment and expire operations correspond to SET, MSET, DEL, INCR and EXPIRE.
A batch is one versioned record; duplicate keys retain last-write-wins
semantics. Decoding rejects malformed/trailing bytes and limits batch item
counts to 65,536. The journal's record and file limits remain configurable.

SET and INCR records contain complete typed after-images. Expiration updates
also store a complete after-image so replay can reconstruct a key whose
earlier deadline passed during downtime. Deadlines are absolute Unix
milliseconds and are converted to a new monotonic deadline on reopen.
Recovery stages a fresh store privately, dropping expired after-images and
applying surviving records in sequence; no partial recovered store is exposed.
Callers must supply coherent monotonic/Unix clock pairs. Wall-clock rollback
can extend or resurrect expired values; clock regression policy is not yet
enforced. Recovery may fail closed if historical replay or arena fragmentation
exceeds admission limits, even when the final logical data set would fit.
Explicit checkpoints can now compact historical replay as described below.

Rejected admission/size/disk-limit requests leave logical data and the
journal unchanged (read-side lazy expiry and counters may still advance).
An ambiguous I/O failure, or failure after a successful append, poisons
the coordinator and blocks further reads and writes until recovery. A
complete record may survive an unacknowledged write, so clients cannot
assume an error means their mutation will never be replayed.

This first implementation prioritizes correctness over copy minimization:
EXPIRE records duplicate the value and may be conservatively rejected by
arena or journal limits; mutation encoding allocates bounded scratch space.
The reactor integration below provides optional durability and shard-wide
fail-stop behavior. Group commit, cross-restart clock
regression handling and physical crash qualification remain separate tasks.


## Optional durable RESP reactor (Unix)

The existing CacheShardServer::bind and bind_with_budget APIs select memory
mode. CacheShardServer::bind_durable and bind_durable_with_budget select a
private journal via CacheDurableConfig (path, cache limits, WAL limits,
WalSync::Buffered or WalSync::Data). Recovery completes before the listener
binds. This is an embedded reactor API; there is no new standalone CLI or
deployment configuration in this change.

The shared RESP executor statically dispatches the memory and durable
backends, using the same command validation and response encoding. The
durable reactor routes topology and MOVED responses before local execution,
reserves bounded response capacity, then executes each local command through
DurableCache. Direct store/store_mut accessors now return Option and expose
only the memory backend. Transparent cross-shard inbox requests are rejected
in durable mode rather than allowed to bypass persistence. This mode remains
redirect-only and Unix-only.

A shard anchors its Unix clock at recovery and advances it with monotonic
time, avoiding in-process wall-clock adjustments. Wall-clock rollback
between restarts remains unresolved. An ambiguous append failure prevents
further responses from the affected command, closes all existing shard
connections, deregisters the listener and returns DurableShardPoisoned;
subsequent poll/run calls reject service. The owning application must drop
and recover the shard before serving again. Non-poisoning admission and
journal-capacity failures return a RESP error without shutting down the
shard. The outcome of a write whose acknowledgement was lost is ambiguous.

Synchronized journal writes block the owning reactor; there is no group
commit or production power-loss qualification yet.
This mode is opt-in. Benchmark the memory and journal modes separately
before selecting a deployment policy.


## Atomic checkpoint and journal rotation (Unix)

DurableCache::checkpoint and CacheShardServer::checkpoint explicitly compact
the journal on the owning thread. The cache enumerates live entries with
borrowed keys and typed values, omits expired/deleted entries, and preserves
absolute deadlines. It writes one bounded after-image per live entry to a
private sibling journal, without cloning the complete data set. The replacement
uses NLCWAL03: a checksummed header declares a base sequence and the required
snapshot-record count. Existing NLCWAL02 journals remain readable; checkpoint
records consume sequence numbers and subsequent writes continue monotonically.

Publication orders complete replacement writes, replacement file sync,
atomic rename over the existing journal, then parent-directory sync. The
original file is untouched before rename. A stable path-based advisory
sidecar lock remains held across inode replacement; never delete the
sidecar while any writer may be active. Use one canonical journal path in a
trusted directory and do not mix older binaries with this format. Failures
before rename preserve the old journal; failures from the rename attempt
onward poison the handle, and the reactor stops serving the shard.

Recovery requires every declared checkpoint record to be present and valid.
A missing or damaged checkpoint is rejected, not treated as a repairable
torn append. Only a subsequent physically incomplete record may be truncated.
Recovery stages the complete cache privately as before. The checkpoint
report exposes live entry count and before/after journal byte lengths.

Checkpointing is synchronous, whether explicit or automatically scheduled;
it blocks the reactor and needs
temporary disk space for both files. A checkpoint may fail if the compact
state (including per-entry framing overhead) exceeds configured journal or
record limits. The old journal is preserved in that case. There is no
background compaction, orphan-temp cleanup
after process termination, or independent snapshot backup. Physical
power-loss qualification and throughput/latency benchmarks remain pending.

The integrated feature-gated RESP test compares raw memory and durable replies
across 768 deterministic commands and 21 checkpoint/reopen cycles, including
expiration, invalid integers, duplicate keys, cross-slot rejection and binary
values. CI runs both the cache and RESP module test suites with cache-server
enabled. This is deterministic logical qualification, not a physical crash or
power-loss test.

## Standalone Unix server

Build and start the feature-gated single-shard executable:

```sh
cargo build --release --locked --no-default-features --features cache-server --bin nulang-fabric
./target/release/nulang-fabric --help
./target/release/nulang-fabric --bind 127.0.0.1:6380 --mode memory
# Use an existing private directory for persistent data.
./target/release/nulang-fabric --bind 127.0.0.1:6380 --mode sync --journal /path/to/private/cache.wal
```

Adjust the binary path if CARGO_TARGET_DIR overrides the default. The server
requires a numeric address and nonzero port. Plaintext binds are restricted
to loopback; remote standalone binds require mutual TLS. Other local users
may connect to a plaintext listener, so do not proxy plaintext to untrusted clients. Durable modes require an
explicit journal path. READY is printed only after recovery and binding.
The server exits nonzero on configuration, recovery, or reactor failure.
Process-level tests kill the child without graceful shutdown and verify that
memory mode loses its contents while both journal modes recover acknowledged
writes after a process-only failure. This does not establish power-loss
survival for buffered writes. Multi-shard configuration, per-client ACLs and production
performance testing remain pending.


## Automatic checkpoint maintenance

Standalone durable modes enable automatic maintenance by default. Embedded
reactors explicitly opt in via enable_automatic_checkpoint. The scheduler
checks journal utilization between poll cycles and individual pipelined
commands. At 70% of the journal limit, it estimates the compacted live
snapshot size without copying entries or creating a replacement file.
It checkpoints only if at least 10% of the current journal can be reclaimed.
Ineffective snapshots or non-poisoning checkpoint errors trigger a five-second
monotonic cooldown. Ambiguous publication still poisons the journal and
stops the shard. Completed/skipped/failed maintenance and reclaimed bytes
are exposed through automatic_checkpoint_stats on the embedded reactor.

Standalone flags: --checkpoint-percent 0..99 (0 disables; default 70),
--checkpoint-min-savings-percent 1..99 (default 10), and
--checkpoint-retry-ms (positive milliseconds; default 5000). These options
are invalid in memory mode. Example:

    nulang-fabric --mode sync --journal /path/to/private/cache.wal --checkpoint-percent 60

Automatic maintenance is not a guarantee that every write fits: a large
individual mutation, a nearly full live data set, a cooldown, or insufficient
temporary disk space can still cause journal-limit errors. The live-entry
size estimate is a linear scan and compaction blocks the reactor. No
background compaction, independent backup, or exported CLI metrics yet.
A process-level regression verifies 100 pipelined writes through a 1 KiB
journal and recovery after killing and restarting the server.


## Graceful shutdown

The standalone Unix executable blocks SIGTERM and SIGINT before journal
recovery and uses a dedicated sigwait thread to request reactor shutdown.
No asynchronous signal handler performs allocation, locking or I/O. Once
the reactor observes shutdown it stops accepting connections, reading input
and executing further commands. It closes the listener, discards unexecuted
input, and attempts to deliver already encoded replies until the configured
network drain deadline. Slow clients and their buffer reservations are then
released. Durable modes explicitly synchronize the journal after draining;
sync failure poisons the journal and returns a nonzero exit status.

The default drain is 5000 ms; --shutdown-drain-ms accepts 0 through 60000
(0 discards pending replies immediately). Embedded owners can use
CacheShardServerControl::shutdown with run() or run_with_shutdown_timeout.
Callers driving poll_once themselves must arrange their own final run/drain
and release the server owner to release the journal lock.

The timeout bounds network draining, not an in-progress mutation,
checkpoint, or disk synchronization syscall. An external service manager
must allow sufficient termination time for storage. A second termination
signal is not a forced-abort mechanism; SIGKILL remains abrupt termination.
A client that loses its response must treat the mutation outcome as
ambiguous. Buffered acknowledgements before a successful final sync still
do not guarantee power-loss durability. The process tests verify successful
SIGTERM and SIGINT exits with incomplete input and journal reopening;
reactor tests cover pending replies, discarded commands, a zero deadline,
a backpressured socket, and injected journal sync failure.


## Durable TTL clock floor

A durable cache now maintains a 48-byte checksummed journal-name.clock
sidecar under the existing stable journal-name.lock writer lock. The
sidecar contains a persisted Unix-millisecond floor covering every guarded observation (an exact observation by default).
At startup, recovery refuses a wall clock earlier than the durable floor.
Correct the operating-system clock before retrying; never reset the floor
to make a rollback start succeed. Startup publishes its clock observation
before replay, including replay that skips expired records. Within a
process, the original wall/monotonic anchor is used; native mutation time
pairs must agree with that anchor.

TTL creation, time-sensitive key access and mutations, removal by expiry
sweeps, checkpoints with surviving TTLs, and orderly reactor shutdown
advance the floor when needed. An observation of a timed key synchronizes
the new floor before returning its result. Persistent-key reads do not
write the clock sidecar. Same-millisecond observations need no extra
publication. Clock publication writes a private temporary file, syncs it,
atomically renames it, and syncs its directory; it does not consume the
bounded mutation journal's capacity. I/O failure during an active clock
publication poisons the cache, and fallible RESP reads stop the shard
rather than exposing stale results or panicking.

Keep the WAL, stable lock file (which now contains an initialization marker),
and clock sidecar together in a trusted private directory. A missing
sidecar with an initialized marker, corrupt marker, or damaged checksum
fails recovery. Existing journals with an empty lock file and no clock
sidecar migrate on first guarded open. They have no historical clock
observations before migration. Do not mix older binaries or restore just
the WAL, delete the lock marker, or restore an older clock sidecar. Back
up all three files consistently with the writer stopped. The checksum
detects accidental corruption, not malicious rollback of an entire backup.

The policy protects against rollback below a *persisted* observation,
not an unobserved time interval lost in an abrupt crash, an erroneous
forward wall-clock jump, or administrative replacement of all metadata.
Timed reads can incur synchronous storage latency, even in buffered mode;
benchmarking and a separately qualified batching strategy remain pending.
Buffered mutation acknowledgements retain their original durability
limitations.

The shutdown drain additionally checks its deadline between socket writes,
not only before each flush, so a continually reading peer cannot extend
the flush loop indefinitely.


## Mutual TLS authentication (standalone)

Use all three PEM options to enable an encrypted listener that requires a
client certificate signed by the configured private client CA:

    nulang-fabric --bind 192.0.2.10:6380 --mode sync --journal /path/to/private/cache.wal --tls-ca /path/to/client-ca.pem --tls-cert /path/to/server-chain.pem --tls-key /path/to/server.key

The example address is a documentation placeholder; use a real local
interface. Without all three TLS options, standalone non-loopback binds
are rejected. Missing/invalid certificate material fails before bind.
The private key is read from a file, not a command-line secret value.
Protect the PEM files and use a dedicated client CA. Clients must verify
the server certificate and its hostname against their trusted server CA.
The client CA may be different from the CA signing the server certificate.

Rustls (existing dependency, explicit ring provider) authenticates the
client during the handshake; there is no plaintext fallback, optional
client certificate mode, early data, or session resumption. Each trusted
client has full access to the supported RESP commands; password AUTH,
per-user/key ACLs, certificate revocation checking and live reload are
not implemented. Rotate credentials by replacing the files and restarting
with the desired trust configuration. Configure a concrete advertised
address when using cluster metadata.

The mio reactor handles encrypted reads and writes nonblockingly and
accounts for pending encrypted output when choosing writable readiness
and draining shutdown replies. TLS mode has at most 256 connections (or
the lower configured limit), a 10-second handshake deadline checked by
the reactor, and a 64 KiB rustls send-buffer target. TLS session state carries a fixed admission reservation in the shared
connection budget (see below); actual allocations and kernel buffers are
not measured by that reservation. This is not an RSS guarantee. Socket close does not guarantee delivery
of pending replies or a TLS close-notify exchange.

The loopback process test generates a private CA and certificates,
rejects unauthenticated and untrusted clients and raw RESP plaintext,
and verifies authenticated fragmented 128 KiB writes and pipelined reads.
Full locked-workspace CI, exact security-baseline integration, WAN load
and adversarial handshake/resource qualification remain required before
production remote exposure. Embedded users must explicitly enable mutual
TLS before polling; the existing low-level plaintext bind API is unchanged.


## Adversarial transport admission qualification

The standalone --max-pipeline-depth option defaults to 1024. The reactor
preflights the number of complete RESP commands in each buffered read
batch before execution, closing an over-limit or malformed batch before
executing its commands. An incomplete trailing frame does not count
until complete. This is a per-buffered-batch limit, not an atomic
transaction or a count of all commands a client has ever sent; TCP/TLS
fragmentation can separate batches and earlier accepted batches remain
committed. The preflight adds a bounded parsing pass.

The reactor now caps its poll timeout by the earliest pending TLS
handshake deadline, independently of the expiration-sweep interval.
Stalled handshakes occupy connection slots until disconnected or timed
out; timed-out and excess connections release admission and buffer
reservations. The deadline is checked in the reactor, not an independent
preemptive timer during synchronous work.

Response serialization is checked against the pre-reserved capacity and
estimated reply size in release builds. A violated estimate closes that
connection before an uncharged vector can remain in the buffer pool.
This is defense in depth, not a guarantee against a transient allocation
if an estimator is wrong; an already executed mutation may have committed.

The loopback tests qualify stalled handshake reclamation, configured
connection admission, exact-depth and fragmented pipeline handling,
authenticated 300 KiB input rejection under a 256 KiB cap, and MGET
amplification rejection under a 512 KiB output cap. A separate existing
authenticated client remains usable after both oversized request and reply
failures. Continuous accept floods, per-peer quotas, aggregate TLS memory,
slow authenticated readers, workload fairness, WAN and long-running load
tests still require independent qualification.


## Cooperative reactor fairness

Each ready token receives at most one turn per reactor poll iteration.
The listener handles at most 32 accept attempts per turn (including
rejected connections and transient accept errors). Each connection gets
up to 32 read operations, 32 RESP commands and 32 write-loop steps per
turn. Plain socket I/O uses at most 16 KiB per application chunk; TLS
work also counts plaintext-reader and encrypted-read operations, and
encrypted writes are driven through Rustls. These are internal quanta,
not externally configurable bandwidth or CPU guarantees.

When a quantum is exhausted, an explicit token-keyed continuation is
coalesced with any kernel readiness for the next iteration. A deferred
turn uses a zero poll timeout so edge-triggered sockets do not require
a new network event to resume. A socket blocked on I/O waits for
readiness rather than spinning; a quantum exhausted immediately before
WouldBlock may require one extra probe. The same scheme reschedules a
full shard-inbox batch. Closed connections and shutdown clear deferred
work. The command preflight is performed once per bounded buffered read
batch; previously validated commands finish before more input is read.
Per-connection reply order is preserved; cross-client ordering is
intentionally unspecified. Pipeline depth remains a per-batch admission
limit, not a transaction or session-wide limit.

Regressions verify that a 40-connection burst admits only 32 on its
first turn then resumes, that an 80-command pipeline yields after 32
commands and another client receives PONG before it finishes, and that
a 768 KiB reply and an encrypted 96-command pipeline resume without a
new client request.

This is cooperative fairness, not strict latency isolation. Parsing
preflight, individual commands (including large MGETs), TLS cryptography,
synchronous journal/clock writes, checkpointing, and a large ready set
can still consume significant wall time. Sustained connection-flood
availability, per-peer quotas, FD exhaustion, aggregate TLS memory and
WAN tail-latency benchmarks are separate gates.


## Shared TLS admission accounting

Each admitted TLS connection reserves 512 KiB against the same shared
connection admission budget that accounts for retained RESP input/output
vector capacity. Reserve before constructing the Rustls session, including
for connections that have not sent a ClientHello. The reservation remains
charged for the connection's entire lifetime, even when its RESP vectors
are empty, and is released after session destruction. Rejected admission
closes the new socket without constructing a TLS session.

The 512 KiB charge is a conservative sizing policy for modeled Rustls
record buffers, handshake state and overhead; it is not a measurement or
proven upper bound on TLS heap allocations. Rustls's 64 KiB send-buffer
target remains in effect. Shared TLS configuration, certificate processing,
allocator overhead, temporary allocations and kernel socket buffers can
consume additional memory. Use OS-level process/container memory limits
and benchmark real workloads before production deployment.

With the default 256 MiB shared budget and 256 TLS connection cap,
256 admitted TLS connections reserve 128 MiB, leaving 128 MiB for
RESP vector charges. Actual admission may be lower if RESP allocations
consume the shared budget. The embedded budget can be shared across
shards. The CLI --max-connection-bytes option includes both charges in
TLS mode; TLS configuration requires a budget greater than a single
512 KiB reservation, and practical deployments require additional
application headroom. Plaintext accounting is unchanged.

CacheBufferBudget::used_bytes includes both charges; limit_bytes returns
the configured shared limit. CacheShardServer::tls_reserved_bytes reports
the modeled reservation for that shard. This is a per-shard snapshot,
not a separate actual-RSS metric. No TLS reservation is released during
idle RESP vector reclamation.

Tests exercise shared two-reactor admission, eight concurrent reservation
attempts competing for three slots, invalid handshakes, timeout,
disconnect, zero-deadline shutdown, insufficient configuration and idle
vector reclamation. Actual TLS allocator profiling, per-peer limits and
file descriptor exhaustion remain separate qualification tasks.


## Per-peer admission and descriptor exhaustion

The default concurrent connection cap per canonical source IP is 64,
configurable with --max-connections-per-peer (positive integer). The
peer lease is acquired before TLS session creation, shared across
reactors using the same CacheBufferBudget, and retained until the
connection is destroyed. IPv4-mapped IPv6 addresses normalize to the
same identity as native IPv4. Zero-count peer entries are removed.
Configure identical peer limits on all reactors sharing a budget;
a reactor with a higher limit otherwise admits more connections under
its own threshold. This is a concurrent source-IP quota, not a rate
limit or authenticated-user identity limit. NATs and trusted reverse
proxies aggregate clients behind their source address; IPv6 address
rotation and distributed sources can evade individual quotas.

If accept encounters EMFILE, ENFILE, ENOBUFS, or ENOMEM on Unix (or
WSAEMFILE/WSAENOBUFS on Windows), the reactor temporarily deregisters
the listener for 100 ms, then re-registers and explicitly retries
acceptance. The poll deadline includes this retry time, so recovery
does not depend on a new edge. Existing connections remain registered
and service continues where possible. Linux subprocess regression
sets RLIMIT_NOFILE to 32, saturates the child descriptor table,
checks PING on an established connection, and verifies new admission
after descriptors become available. The parent process limit is
unchanged. This protects the accept path, not all descriptor-dependent
operations: durable clock publication, journal/checkpoint I/O or poll
registration failures may still cause errors or fail-stop. No claim
of sustained distributed-flood resistance or Windows qualification.


## Operational metrics and health

The standalone process optionally exposes a separate loopback HTTP
listener using --metrics-bind 127.0.0.1:9090 (IPv6 loopback is also
supported). It is disabled by default. The endpoint is intentionally
local and unauthenticated; any local process can read aggregate
statistics. Do not publish it to an untrusted network through a proxy.
A monitoring agent on the host can scrape GET /metrics, which returns
Prometheus-compatible text exposition with fabric_ metric names.
GET /healthz and GET /readyz return 200 when healthy/ready, or 503
otherwise. Unknown paths return 404. There is no command or key access
through this interface, and no high-cardinality peer, key or client labels.

The owning reactor publishes an immutable numeric snapshot every
250 ms when observation is enabled. A dedicated listener thread
copies the sample under a short mutex, never dispatches commands,
and handles one request at a time with a 1024-byte request-header cap
and 250 ms combined I/O deadline. Publication uses try_lock so a
scrape cannot block the data reactor. A sample older than two seconds
is stale; both health probes fail closed and fabric_snapshot_fresh
is zero. Readiness is false during listener resource-exhaustion
backoff, poison or shutdown. A healthy snapshot only attests to
recent reactor progress, not ability to persist the next mutation;
a full journal or long synchronous checkpoint can affect availability.
Long checkpoints can make a healthy process appear stale. Tune
external probe failure thresholds accordingly; avoid restart loops.

Metrics include active and accepted connections, rejected connections
by limit/peer/TLS budget, listener resource errors, connection-drive
errors, handshake timeouts, dispatched commands, completed poll
iterations, shared connection-budget used/limit bytes, TLS reservations,
physical cache entries, arena and approximate retained capacity,
store hits/misses/expirations, durable mode, journal usage/limit and
automatic checkpoint completion/failure/skips/reclaimed bytes.
The command count includes dispatched error/redirect responses and
does not attest to client receipt or mutation commitment. It excludes
native direct store access and inbox dispatch. Store counters may
include internal lookups and replay; physical entries may include
expired entries pending removal. Automatic checkpoint counters do
not include explicit manual checkpoints. Counters reset on restart.
Shared budget gauges are process-wide when the same budget is
shared by multiple reactors; do not sum them across those shards.
TLS charges and cache capacity estimates are not measured RSS.
Embedded users can obtain operational_snapshot on the owning
thread, or use run_observed with a trusted nonblocking callback.

Qualification covers real memory-mode commands and peer rejection,
durable checkpoint progress, stale and degraded probe responses,
loopback-only configuration and an incomplete local probe concurrent
with healthy RESP traffic. Metrics availability under process-wide
descriptor exhaustion is not guaranteed. No external monitoring
deployment, latency histogram or production alerting policy is implied.


## Opt-in durable clock reservations

Use --clock-reservation-ms 0..1000 in durable standalone mode, or call
DurableCache::configure_clock_reservation in embedded mode. The default
zero keeps exact-floor publication. When an observation exceeds the
durable watermark, publish the observation plus the configured reservation
(with saturating arithmetic), using the existing atomic file replacement
and directory synchronization protocol. Subsequent guarded observations
at or below the watermark need no storage write. Track the last actual
observation separately; an in-process backwards observation still poisons
the cache. Reservation does not alter TTL deadlines, monotonic elapsed
time, WAL framing, or buffered/synchronous acknowledgement policies.

Recovery always refuses wall time below the persisted watermark,
including a reservation beyond the last actual observation. This can
delay a restart by up to the configured window relative to the anchored
clock, even after orderly shutdown. Clock anomalies may cause a longer
delay. Do not bypass the check or delete the sidecar to recover sooner.
Changing the policy to zero does not lower a previous persisted
watermark. Renewal errors poison the cache before a result is returned.
The existing on-disk format remains unchanged; older cooperating
exact-floor readers will also refuse a premature restart.

Tests cover renewal boundaries, unchanged sidecar during the reserved
interval, TTL expiration and non-resurrection, early-restart refusal,
in-process backwards observations, publication failure and CLI bounds.
See benchmarks/FABRIC_RESP.md for the follow-up measurement and its
limitations. Per-command durable WAL synchronization is unchanged.
