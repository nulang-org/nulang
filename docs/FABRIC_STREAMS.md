# Nulang Fabric Streams

Fabric Streams are the durable-log layer above Fabric's ephemeral topic routing.
This first implementation deliberately establishes the local storage contract
before adding partition replication or broker-compatibility semantics.

## Implemented storage contract

`FileFabricStreamStore` provides an append-only segmented log rooted at a caller
selected directory.

Each stream has:

- `meta.json` — format version and segment-size configuration.
- `<base-sequence>.seg` — ordered immutable-history segments.
- `cursors.json` — atomically replaced consumer cursor state.

Records are assigned monotonically increasing 64-bit sequence numbers starting
at 1. A record frame contains:

1. sequence number,
2. payload length,
3. a 128-bit truncated BLAKE3 checksum over sequence + payload,
4. opaque payload bytes.

The payload is intentionally opaque at this layer. Future typed `stream`
declarations can choose their serialization without changing the durable-log
format.

### Durability

A successful append writes the complete frame, flushes it, and calls
`sync_data` before returning its sequence number.

Metadata and cursor updates use write-to-temp + `sync_all` + atomic rename.
The containing directory is synchronized after replacement.

### Crash recovery

Opening a store is cheap; an individual stream is recovered lazily on first
access. Recovery scans segments in base-sequence order and verifies:

- segment magic and format version,
- filename/header base-sequence agreement,
- sequence continuity across every record and segment,
- per-record checksum integrity.

An incomplete final frame is treated as a torn tail and truncated to the last
complete record. Sequence gaps or checksum mismatches fail closed rather than
silently discarding committed history.

### Segment rotation

`FabricStreamConfig::segment_max_bytes` controls rotation. Rotation happens
before appending a frame that would exceed the configured size, except that one
record larger than the target is allowed to occupy its own segment.

### Consumer cursors and replay

A cursor is the last fully processed stream sequence for a named consumer.
Cursor commits are monotonic and cannot advance beyond the current stream tail.

`read_consumer(stream, consumer, limit)` replays from `cursor + 1`.

This establishes the persistence primitive needed for later ACK/NACK semantics
without pretending that ACK redelivery already exists.

## Runtime APIs

After opening storage:

```rust
runtime.fabric_stream_open("/var/lib/nulang/fabric")?;
runtime.fabric_stream_create("orders", FabricStreamConfig::default())?;
let sequence = runtime.fabric_stream_append("orders", payload)?;

let records = runtime.fabric_stream_read("orders", 1, 100)?;
runtime.fabric_stream_commit_cursor("orders", "billing", sequence)?;
let pending = runtime.fabric_stream_read_consumer("orders", "billing", 100)?;
```

Available APIs:

- `fabric_stream_open`
- `fabric_stream_create`
- `fabric_stream_append`
- `fabric_stream_read`
- `fabric_stream_read_consumer`
- `fabric_stream_commit_cursor`
- `fabric_stream_cursor`
- `fabric_stream_info`

## Explicitly not implemented yet

This storage PR does **not** claim distributed stream semantics. Follow-up work
must add:

1. partition ownership and replica placement,
2. replicated append / quorum policy,
3. ACK/NACK and timed redelivery,
4. retention by age/bytes/sequence,
5. dead-letter streams,
6. producer deduplication/idempotency keys,
7. consumer groups and partition assignment,
8. sequence/time seek indexes,
9. typed language-level stream declarations,
10. NATS JetStream compatibility only after native semantics stabilize.

The architectural boundary is intentional: the append log should remain usable
for embedded/single-node Nulang even when cluster replication is disabled.


## Deterministic replica placement

The replication stack adds deterministic rendezvous placement over the cluster's
stable known-membership set.

For a `(stream, partition)` pair, every node independently computes the same
ordered replica set from:

- stream name,
- partition id,
- each known node's stable `NodeId`.

The first replica is the leader. The current physical storage implementation is
still one partition per stream, so replicated appends accept `partition = 0`
only. The placement type already carries a partition id so later physical
multi-partition logs do not need a new ownership contract.

### Split-brain safety

Placement intentionally includes `Suspicious` and `Failed` members until
cluster membership confirms them removed. A transient network partition
therefore does **not** move leadership just because one side stopped hearing the
current leader.

If the designated leader is unavailable, another node does not self-elect and
write. The append fails closed. Leadership can change after confirmed removal,
which is already guarded by the cluster's removal/quorum machinery when a
split-brain resolver is configured.

This is deliberately stricter than an availability-first broker. An explicit
monotonic stream epoch / lease protocol is required before automatic failover
can safely be added.

### Replica append envelope

A leader-local append can produce a `FabricStreamReplicaAppend` containing:

- stream + partition,
- leader NodeId,
- stable-membership fingerprint,
- replication factor,
- exact leader-assigned sequence,
- stream storage configuration,
- payload.

A receiving replica recomputes placement before accepting the envelope. It
rejects:

- stale membership fingerprints,
- a leader that no longer matches deterministic placement,
- delivery to a node outside the replica set,
- sequence gaps,
- conflicting duplicate sequences,
- storage-configuration mismatch.

Identical duplicate delivery is idempotent.

This is the replica **data contract**, not yet the network/quorum protocol.
Remote transport, ACK collection, and commit-quorum semantics remain follow-up
work.


## Replica transport over NUL0 v1

Replica data now has a backward-compatible transport path without adding a new
NUL0 packet discriminant.

Fabric reserves actor id `0` (already not a user actor) plus the internal
behavior name `__nulang_fabric_stream_replica_v1`. A replica envelope is
serialized into one object-table entry of the existing `Packet::ActorMessage`
wire shape and sent at System priority.

The receiver intercepts this reserved system message before ordinary actor
lookup and validates:

- the packet's declared sender NodeId equals the transport-authenticated peer,
- the envelope leader equals that peer,
- exactly one object-table entry with id 0 is present,
- the envelope decodes within the configured size bound,
- current deterministic placement still accepts the leader/replica set,
- exact-sequence durable application succeeds.

This keeps `WIRE_VERSION = 1` and packet discriminants unchanged.

`FabricStreamReplicaDispatchReport` distinguishes intended remote replicas,
packets dispatched to reachable members, and currently unavailable replicas.

**Dispatch is not commit.** The existing NUL0 packet ACK only confirms packet
processing at the transport layer. It is not an application-level replica fsync
acknowledgement and is not counted as quorum durability. The next layer must add
application ACKs and a committed sequence/index before Fabric Streams can expose
quorum-committed records.


## Application ACKs and quorum commit

Fabric Stream replication now separates three different events that must not be
conflated:

1. **transport dispatch** — the leader handed a replica packet to NUL0,
2. **replica application ACK** — the follower validated placement and fsynced
   the exact sequence,
3. **quorum commit** — a majority of the configured replica set has application
   ACKed the sequence.

The leader counts its own durable local append as the first application ACK.
For replication factor `N`, quorum is `floor(N / 2) + 1`.

Followers return a reserved application-level ACK/NACK system message only
after attempting exact-sequence durable application. NUL0's existing
`Packet::Ack` remains transport-only and is never counted toward stream
quorum.

### Pending tickets

`fabric_stream_replicated_append` creates a leader-local pending ticket with:

- stream + partition + sequence,
- leader and membership fingerprint,
- allowed replica set,
- majority quorum,
- unique application ACK set,
- unique rejection/NACK set.

Duplicate follower ACKs are idempotent. A later positive ACK replaces an
earlier rejection from the same replica. ACKs from nodes outside the replica
set, ACKs for another leader, and ACKs from an obsolete membership fingerprint
are rejected.

A delayed ACK cannot commit a still-pending record after confirmed membership
changes: the leader recomputes placement against the active `ClusterState`
before accepting the ACK.

### Contiguous commit index

Quorum readiness is not enough to create a hole in committed history. The
leader advances the committed boundary only through the longest contiguous
prefix where every next sequence has quorum.

The committed sequence is persisted in `commit.json` using the same atomic
temp-file + fsync + rename discipline as cursors. It therefore survives leader
restart independently from the in-memory pending tickets.

`fabric_stream_read_committed` exposes only records at or below this durable
boundary. The ordinary `fabric_stream_read` API remains a raw local-log read
and can include uncommitted replica/leader tail records.

### Restart model

Pending replication tickets are currently in-memory only. If a leader restarts:

- already committed history remains committed because `commit.json` is durable,
- locally durable but uncommitted tail records remain in the raw log,
- those uncommitted records are **not** promoted to committed automatically,
- later catch-up/retry work must reconstruct or retry pending replication.

This is fail-closed: restart cannot turn a locally durable write into a quorum
commit by accident.

### Follower visibility

Followers durably store replica records before ACKing, but they do not yet
receive a separate committed-index propagation message. Consequently, the
authoritative committed boundary in this slice is leader-local.

Commit-index replication / follower committed reads belong with the upcoming
catch-up and failover protocol.

### Runtime APIs

- `fabric_stream_replicated_append`
- `fabric_stream_replication_status`
- `fabric_stream_committed_sequence`
- `fabric_stream_read_committed`

This ACK layer concerns **replica durability**, not consumer delivery. Consumer
ACK/NACK, timed redelivery, dead-letter streams, and consumer groups remain
separate future work.


## Durable replication intent and restart recovery

Fabric now persists a `replication.json` intent record for every leader
sequence that requires replica quorum.

The write ordering is deliberately:

1. persist replication intent,
2. fsync the leader's exact stream record,
3. create the in-memory quorum ticket,
4. dispatch replicas.

That ordering closes the previous restart gap. A process can no longer leave a
locally durable uncommitted sequence without enough metadata to reconstruct its
replica set and quorum requirements.

Each durable intent records:

- partition,
- leader NodeId,
- membership fingerprint,
- replication factor,
- ordered replica set,
- exact sequence.

The payload itself is not duplicated in `replication.json`; recovery reads the
already-checksummed stream record at the exact sequence.

### Recovery rules

`fabric_stream_recover_pending(stream)` reconciles durable intent with the
stream log and committed boundary:

- intent sequence <= committed boundary: remove stale intent,
- intent at the next sequence with no durable record: treat it as a crash
  between intent reservation and append, then remove the orphan reservation,
- intent below the local tail with no matching record: fail closed as durable
  corruption,
- current leader / membership fingerprint / ordered replica set differs from
  the persisted intent: fail closed rather than reinterpreting the write under
  a new placement,
- valid uncommitted record: rebuild the in-memory ticket with only the leader's
  self-fsync ACK.

Follower ACK history is intentionally not reconstructed. The leader cannot
prove which followers persisted a sequence before the crash, so recovery
forgets those ACKs and retries safely.

### Retry

`fabric_stream_retry_pending(stream, partition)` reconstructs durable tickets
first and then redispatches every pending exact sequence to the current persisted
replica set.

Retry may resend to a follower that already has the record. Exact-sequence
replica application makes that safe and idempotent; the follower simply ACKs
the identical record again.

The retry report exposes:

- pending sequence count,
- intended remote sends,
- dispatched sends,
- currently unavailable replicas.

If an application ACK arrives immediately after leader restart before the user
explicitly invokes recovery, the ACK path reconstructs the matching durable
ticket against the active `ClusterState` before evaluating it.

### Commit cleanup ordering

When quorum advances:

1. persist the committed sequence,
2. remove the matching durable replication intent,
3. retire the in-memory ticket.

A crash after step 1 but before step 2 is harmless: recovery sees that the
intent is already at or below the durable committed boundary and removes it.

### Remaining recovery work

This layer recovers **uncommitted leader tail**. It does not yet repair a
replica that missed a record already committed by a majority. That requires
per-replica progress / catch-up state and committed-index propagation.

Automatic timer-based retry is also intentionally deferred; retry is currently
explicit so failure/recovery semantics can stabilize before adding scheduling.


## Lagging committed-replica catch-up

Fabric now tracks a durable leader-side high-water mark for every follower that
successfully application-ACKs a stream record.

`replica_progress.json` stores:

- replica NodeId,
- highest exact sequence durably ACKed by that replica.

Progress is monotonic and persisted before an accepted ACK is allowed to affect
leader bookkeeping. Losing progress metadata is safe but conservative: the
leader can resend older committed records because exact-sequence replica
application is idempotent.

### Catch-up

`fabric_stream_catch_up_committed(stream, partition, replication_factor, max)`
repairs followers using only the leader's durable committed prefix.

For each remote replica:

1. read its persisted progress,
2. compare progress with the leader committed sequence,
3. read at most `max` missing committed records,
4. dispatch those exact sequences only to that replica,
5. wait for normal application ACKs to advance durable progress.

No uncommitted leader tail is used for catch-up.

The bounded record count prevents a single repair call from monopolizing the
runtime for a severely lagging replica.

### Commit-boundary propagation

Replica data and commit visibility remain separate.

Fabric uses the reserved internal behavior
`__nulang_fabric_stream_commit_v1` to propagate a leader's durable committed
sequence without adding a NUL0 packet discriminant.

A commit update carries:

- stream + partition,
- leader NodeId,
- membership fingerprint,
- replication factor,
- committed sequence.

A follower accepts the update only when current deterministic placement still
matches and its local durable tail is at least the advertised committed
sequence. The follower then persists its own `commit.json` boundary.

The leader never sends a commit boundary merely because a packet was
dispatched. It sends commit updates only to replicas whose persisted application
ACK progress proves they already contain that committed prefix.

Whenever an application ACK is processed, the leader checks all replicas whose
durable progress has reached the current commit boundary and sends them a commit
update. This covers:

- followers that formed the original quorum,
- a follower catching up after a partition,
- duplicate/idempotent catch-up ACKs.

If a replica was unavailable for the update, a later explicit catch-up call
retries the commit boundary when its recorded progress is already sufficient.

### Current safety boundary

Catch-up repairs data and committed visibility under the existing deterministic
leader placement. It does **not** move leadership.

A membership change that changes placement still requires a future monotonic
leader epoch/lease protocol before automatic failover can safely reinterpret
stream ownership.


## Automatic pending-quorum retry

Pending **uncommitted** Fabric Stream replication now retries automatically from
the runtime network loop.

The retry scheduler uses `Runtime::now()`, so production uses the monotonic
wall clock and deterministic tests use the installed virtual clock.

The schedule is exponential:

- first retry: 500 ms after the initial dispatch,
- then 1 s,
- 2 s,
- 4 s,
- continuing up to a 30 s cap.

A retry redispatches each still-pending exact sequence using the existing
idempotent replica-append contract. The scheduler never converts transport
dispatch into an ACK; quorum still advances only from follower application ACKs.

When a partition's pending ticket set becomes empty after commit, its retry
schedule is removed immediately.

Restart recovery remains driven by durable replication intent. Once
`fabric_stream_recover_pending` reconstructs a ticket (explicitly or because an
incoming ACK triggers reconstruction), an immediate retry schedule is installed.

This scheduler targets **pending quorum work** only. Repairing followers that
missed records already committed by a majority remains the bounded
`fabric_stream_catch_up_committed` path, because bulk catch-up needs separate
workload/rate controls.


## Durable replication policy and epoch fencing

Every replicated Fabric Stream now has a durable `replication_policy.json`
record. The initial policy is established atomically as **epoch 1** before the
first replicated leader append.

The policy records:

- partition,
- monotonic epoch,
- leader NodeId,
- membership fingerprint,
- replication factor,
- ordered replica set.

Once established, the durable policy is the authority for the normal stream
data plane. Append, retry, recovery, replica ACK validation, committed catch-up,
replica application, and committed-index propagation use the policy's exact:

- epoch,
- leader,
- ordered replica set,
- replication factor,
- membership fingerprint.

Current cluster-wide rendezvous placement is **not** recomputed for those
operations. Consequently, an unrelated node joining the cluster, a previously
excluded node rejoining, or other membership growth cannot implicitly rebalance
an established stream or invalidate otherwise-current ACK/commit traffic.

Current cluster membership still matters for **reachability**: an installed
replica that is unavailable cannot be dispatched to and quorum may therefore be
unavailable. Changing stream ownership or replica membership, however, requires
an explicit higher-term transition that installs a new durable policy.

Rendezvous over current membership is now a candidate-selection mechanism used
for first epoch-1 bootstrap and explicit reconfiguration/failover, not the
ongoing source of truth for an already-established stream.

### Epoch-carrying protocol

The following internal messages/state now carry an epoch:

- durable pending replication intent,
- replica append envelope,
- follower application ACK/NACK,
- quorum pending ticket,
- committed-index update.

Epoch fields are additive JSON fields inside the existing reserved NUL0 v1
`ActorMessage` envelopes. Missing epoch fields deserialize as epoch 1 for
compatibility with the immediately preceding experimental stream stack. Epoch 0
is always invalid.

A follower may bootstrap a missing policy only for epoch 1 and only when the
local stream has no durable history. New replica-append envelopes carry the
complete ordered replica set from the leader's durable policy. On first contact,
the follower validates:

- epoch is exactly 1,
- replica count matches the advertised replication factor,
- the leader is the first ordered replica,
- replica IDs are unique,
- the local node belongs to the carried replica set.

The follower then persists that exact carried policy before applying the record.
First contact therefore no longer depends on the follower's contemporaneous
global membership view; membership can change between leader policy creation
and first follower delivery without rebinding the stream.

The ordered replica field is additive. An older experimental envelope that does
not contain it still decodes with an empty replica list and uses the legacy
rendezvous bootstrap path for compatibility. Once a policy exists, any non-empty
carried replica list must exactly equal the installed ordered policy.

A stream with existing records but no policy is rejected and requires explicit
migration; ownership is never inferred retroactively from today's membership.

### Stale traffic fencing

Once epoch 1 is persisted, an otherwise valid append, ACK, or commit update with
epoch 2 is rejected. The inverse will apply after future transitions: once a
higher durable epoch is installed, delayed traffic from an older term will be
rejected before it can affect progress or committed visibility.

### Introspection

`fabric_stream_epoch(name)` returns the currently persisted epoch, or `None`
before replication policy has been established.

### Deliberate limitation

This layer does **not** increment epochs and does not move leadership.

The next ownership layer must implement a quorum-backed epoch transition that
proves the prospective leader has the committed prefix before atomically
installing a higher epoch. Automatic failover remains disabled until that
protocol exists.


## Quorum-backed epoch transition

Fabric now has an explicit transition protocol for moving a stream from an
installed epoch to a strictly higher election term without allowing the old
epoch to continue forming commits.

The public entry points are:

- `fabric_stream_begin_epoch_transition(stream, partition, new_replication_factor)`
- `fabric_stream_resume_epoch_transition(stream)`

The first transition protocol is deliberately conservative.

### Durable promise

Before a replica casts an affirmative epoch vote it fsyncs
`epoch_promise.json` containing:

- promised epoch,
- deterministic proposal hash.

A promise for epoch `N+1` immediately fences normal stream traffic from epoch
`N` on that node. Repeating the same proposal is idempotent. A conflicting
proposal for the same epoch is rejected.

A strictly higher term may supersede an abandoned lower promise even when the
installed policy has not advanced yet. This mirrors consensus election terms:
a stalled epoch 2 proposal can be abandoned in favor of epoch 3, and the epoch
3 promise fences both epoch 1 and epoch 2 traffic.

This creates the key failover invariant:

> once an old-policy majority promises the next epoch, the old leader can no
> longer obtain an old-epoch commit quorum.

### Proposal

A transition proposal binds, through a BLAKE3 proposal hash:

- stream name,
- complete old policy,
- complete proposed policy,
- candidate durable tail.

The prospective leader must be:

- the current deterministic leader of the proposed placement,
- a member of the old replica set.

For this first protocol, every proposed new replica must also be a member of the
old replica set. The protocol therefore supports safe shrink/leadership changes
among existing replicas, such as RF=3 -> RF=2 after one confirmed loss, but does
not yet introduce a replacement node.

### Exact-prefix vote

An old-policy replica votes yes only when its local durable tail is **exactly**
the candidate's durable tail.

A shorter replica votes no and does not create a promise. That rejection can be
replaced by a yes vote later after the replica is repaired to the same proposal
tail. An affirmative vote is immutable.

Requiring equal tails avoids carrying an uncommitted hole across the ownership
boundary.

### Finalization

The candidate finalizes only when:

1. affirmative votes are at least a majority of the old replica set,
2. every member of the proposed new replica set is among those affirmative
   voters,
3. those votes all certify the exact candidate tail.

The full candidate tail is then quorum-durable under the old policy and becomes
the committed boundary of the new epoch.

Finalization order is crash-safe:

1. persist finalized transition state and quorum-certified boundary,
2. install the new durable policy,
3. advance the durable committed boundary,
4. retire old-epoch pending intents/tickets,
5. send epoch-commit messages to the new replica set.

If the candidate crashes after step 1, `fabric_stream_resume_epoch_transition`
idempotently completes the remaining local steps and re-sends the commit.

### Epoch commit

A new replica accepts the transition commit only when:

- sender is the proposed new leader,
- it belongs to the proposed new replica set,
- current deterministic placement still equals the proposed policy,
- the voter set is unique, belongs to the old replica set, and reaches old
  quorum,
- every new replica appears in that affirmative voter set,
- the local durable promise matches the proposal,
- the local durable tail exactly equals the quorum-certified tail.

It then installs epoch `N+1` and persists that committed boundary.

### Monotonic election terms

Installed policies move only forward, but abandoned election terms may be
skipped. A non-finalized proposal can be superseded only by a higher term with
the same installed `from_policy`. A finalized transition can be replaced only
by a proposal whose `from_policy` is exactly the previously installed
`to_policy`.

This prevents an interrupted election from permanently wedging the stream while
still forbidding rollback to an older term.

### NUL0 compatibility

Prepare, vote, and commit use three reserved system behavior names carried by
the existing NUL0 v1 `ActorMessage` envelope:

- `__nulang_fabric_stream_epoch_prepare_v1`
- `__nulang_fabric_stream_epoch_vote_v1`
- `__nulang_fabric_stream_epoch_commit_v1`

No packet discriminant or `WIRE_VERSION` change is required.

### Threat model

This protocol provides crash/partition fencing for the current Nulang cluster
model. It is not Byzantine consensus: the runtime-generated commit certificate
contains transport-authenticated voter identities but no cryptographic
signatures.

### Remaining ownership work

Automatic leader failover is still disabled. The next layer can use this
protocol to trigger transitions after confirmed failure, but it must preserve
the exact-tail requirement and avoid repeatedly shrinking replication factor
without explicit policy.


## Proposal-scoped repair for lagging epoch voters

A proposed new-policy replica may be behind the prospective leader even though
both survived the old leader. Fabric can now repair that voter without
re-enabling ordinary traffic from the fenced old epoch.

New API:

`fabric_stream_repair_epoch_transition(stream, max_records_per_replica)`

The candidate reads the durable transition state and considers rejected voters
that are members of the proposed new replica set.

For a rejected voter whose durable tail is below the proposal's candidate tail:

1. read a bounded exact suffix from the candidate's local durable log,
2. send it through reserved behavior
   `__nulang_fabric_stream_epoch_repair_v1`,
3. receiver validates stream identity, proposal hash, old durable policy,
   proposed placement, target identity, and promise fencing,
4. receiver applies the exact missing sequences,
5. receiver immediately re-evaluates the same prepare proposal,
6. receiver returns an updated rejection (partial repair) or an affirmative
   durable vote (fully caught up).

Repair batches are idempotent. Already-applied records are compared against the
local durable sequence and payload; only the missing suffix is appended.

The repair channel is intentionally separate from normal replica append traffic.
A node that has promised a higher election term never needs to weaken that
promise in order to reconcile data.

### Bounded multi-round repair

The caller supplies a per-replica record bound. If a voter is more than one
batch behind, its updated rejection reports the new tail and a later repair call
continues from there.

### Candidate-behind case

If a rejected voter is ahead of the prospective leader,
`FabricStreamEpochRepairReport.ahead_replicas` reports it and no records are
modified.

Pulling old-policy data from an ahead survivor into an already-fenced candidate
requires a separate proposal-scoped pull/reconciliation protocol. Once the
candidate tail changes, it must start a higher election term because the old
proposal hash binds the earlier tail.


## Proposal-scoped pull reconciliation for an ahead survivor

The inverse transition-repair case is now supported: the deterministic
prospective leader can be behind another surviving proposed replica.

New API:

`fabric_stream_pull_epoch_transition(stream, max_records)`

The candidate uses the active transition's rejected votes to identify proposed
replicas whose durable tail is ahead of its own proposal tail. It selects the
highest reported tail deterministically and sends a bounded request through:

- `__nulang_fabric_stream_epoch_pull_request_v1`
- `__nulang_fabric_stream_epoch_pull_response_v1`

### Source validation

The source accepts a request only when:

- requester is the proposal's prospective leader,
- source is the addressed proposed replica,
- durable installed policy still equals the proposal's old policy,
- current deterministic placement still equals the proposed new policy,
- no newer/conflicting durable promise fences the proposal,
- the requested start sequence is within the source's durable tail.

The source returns raw exact-sequence records plus its current durable tail.

### Candidate validation

The candidate accepts a response only when:

- it still owns the exact active proposal,
- sender is a proposed replica,
- installed old policy and current proposed placement still match,
- the source's returned tail equals the tail recorded in its rejected vote,
- the candidate local tail is still exactly the proposal's original candidate
  tail,
- the response begins at the candidate's next sequence and contains no gaps.

The response is then appended to the candidate's durable log through the
transition-repair append path.

### Mandatory higher term after pull

Applying even one pulled record changes the candidate tail. Because the proposal
hash binds that tail, the existing election proposal becomes stale immediately.

The candidate therefore does **not** install the old proposal after pulling.
The caller starts `fabric_stream_begin_epoch_transition(...)` again. Durable
promise state selects a strictly higher term, whose proposal hash binds the new
candidate tail.

Example:

```text
installed epoch 1
candidate proposes term 2 at tail 1
peer rejects with tail 2
candidate pulls sequence 2
term-2 proposal is now stale
candidate starts term 3 at tail 2
peer matches tail 2 and votes yes
term 3 installs
```

For bounded pulls where the source remains ahead after one batch, the same
process repeats through higher terms. This is intentionally conservative:
candidate log mutation and election identity are never hidden inside the same
proposal.

### Safety boundary

Pull reconciliation trusts durable records held by an old-policy replica under
Nulang's crash/partition trust model. After the candidate copies the suffix, an
old-policy majority can hold that exact tail and certify it in the higher term.

Fabric stream records do not yet persist per-record origin term metadata. A
future log-format upgrade can strengthen provenance validation before expanding
the threat model beyond trusted cluster replicas.


## Automatic failover after confirmed leader removal

Fabric can now automatically initiate a safe ownership transition when the
cluster marks the durable stream leader **confirmed removed**.

The trigger is deliberately attached to the same confirmed-gone boundary used
for durable actor respawn. A merely `Failed`/suspected/partitioned node never
starts stream failover.

Confirmed removals are queued while network packet processing may temporarily
own `ClusterState` outside `Runtime`. The queue is drained only at the end of
`process_network()`, after runtime-owned transport and cluster state are
restored.

### Eligibility

For every durable stream whose installed policy names the removed node as
leader, automatic orchestration requires:

1. surviving members of the old replica set still reach the **old-policy
   majority quorum**,
2. the reduced replication factor equals the number of surviving old replicas,
3. current deterministic placement for that reduced factor contains exactly
   those surviving old replicas,
4. the local node is the deterministic leader of that placement.

If current placement would introduce a node outside the old replica set,
automatic failover skips the stream. Replica-set expansion remains a separate
reconfiguration protocol.

Likewise, RF=2 with one removed replica cannot auto-fail over because the single
survivor does not constitute the old RF=2 quorum.

### Stream discovery

The file-backed store now exposes `fabric_stream_names()`, discovered from
durable stream directories containing `meta.json`. This lets failover
orchestration recover its scope after process restart rather than depending on
an in-memory stream registry.

### Automatic prepare retry

Confirmed-removal observations are not guaranteed to become visible on every
survivor in the same scheduler turn.

A candidate may therefore send an epoch prepare before another survivor has
locally removed the old leader. That peer correctly rejects the proposal because
its deterministic placement still differs.

Active candidate transitions now retry the same durable proposal using
`Runtime::now()`:

- first retry after 500 ms,
- then 1 s,
- 2 s,
- 4 s,
- exponential backoff capped at 30 s.

The schedule is in-memory, while durable transition state is the restart source
of truth. After restart, a local candidate reconstructs an immediate retry entry
from the transition file.

### Current automatic scope

When survivors already share the exact durable tail, the full path is now
automatic:

```text
leader confirmed removed
        ↓
old quorum still survives
        ↓
deterministic surviving candidate
        ↓
prepare / durable promises / votes
        ↓
old-policy quorum certificate
        ↓
higher epoch installed
```

If survivor tails differ, the transition remains safely pending. The
proposal-scoped push and pull APIs can repair/reconcile the mismatch; automatic
selection of those repair actions is a separate orchestration layer.


## Automatic divergence repair during confirmed-removal failover

Automatic failover now drives proposal-scoped reconciliation instead of
stalling when the surviving replicas have different durable tails.

The existing logical-clock failover retry loop examines the durable active
transition and chooses exactly one bounded action per due retry:

1. **candidate tail changed** — start a strictly higher election term whose
   proposal hash binds the new local tail,
2. **a proposed voter is ahead** — issue one bounded proposal-scoped pull,
3. **a proposed voter is behind** — issue one bounded proposal-scoped push
   repair,
4. **no known tail divergence** — resend the durable prepare proposal.

Automatic repair uses at most 256 records per stream per retry tick. It never
loops synchronously inside `process_network()`; normal logical-clock backoff
still controls the next action:

- 500 ms,
- 1 s,
- 2 s,
- 4 s,
- capped at 30 s.

### Candidate ahead

When the deterministic failover candidate has the longer durable tail, a
rejected survivor reports a lower tail. The scheduler invokes
`fabric_stream_repair_epoch_transition` automatically. The survivor applies
the exact proposal-scoped suffix and immediately re-votes. If the batch reaches
the candidate tail, the existing quorum transition can finalize without a new
term.

### Candidate behind

When a rejected survivor reports a higher tail, the scheduler invokes
`fabric_stream_pull_epoch_transition` automatically. The candidate applies
the exact suffix but does not finalize the stale proposal. On the next due retry
the scheduler sees that local tail no longer equals the proposal-bound
candidate tail and starts a higher term automatically.

The higher proposal then binds the reconciled tail and proceeds through normal
prepare/vote/quorum finalization.

### Bounded convergence

If more than 256 records are missing, the same mechanism converges across
multiple retry ticks. Push repair advances the rejected voter's reported tail;
pull reconciliation advances the candidate tail and therefore intentionally
uses a higher term for the next proposal.

This keeps failover recovery bounded, deterministic, and observable while
preserving the invariant that a proposal hash always identifies one exact
candidate log prefix.
