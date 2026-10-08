# Fabric replicated consumer progress: safety contract and implementation plan

**Status:** Private journal (#1445), follower prepare/receipt transport
(#1447), leader-local metadata quorum commits (#1448), and a separate
**follower COMMIT-fsync confirmation / bounded history replay slice** on
`feat/fabric-consumer-commit-confirmation-20261008`.
**Cluster-durable client ACKs, leader-failover metadata recovery, and
cryptographically authenticated quorum certificates are NOT implemented.**
The public consumer cursor, ACK gap, and lease APIs still use node-local
storage. Draft #1432 checks the locally installed leader epoch but does
not prove live quorum.

### Experimental transport slice

The leader can stage a private metadata prepare and dispatch it over the
existing NUL0 system-message path. Followers validate the transport-visible
sender against the message identity and the locally installed stream leader,
reject different policies or higher promised epochs, ensure their committed
stream data covers the proposal, and fsync the exact pending metadata change
before returning an application-level receipt. The leader validates the
transport-visible replica identity, installed membership/epoch, pending
metadata sequence and exact BLAKE3 proposal digest before recording a receipt
by appending a fsynced `Receipt` record to its local hash-chained
journal. Duplicate deliveries are idempotent and cannot inflate the quorum.

### Experimental journaled quorum ticket and commit propagation

The leader's own fsynced `Prepare` is vote one. Each transport-validated
follower fsync receipt appends a separate durable `Receipt` record scoped
to the exact proposal digest, stream, metadata sequence, and replica ID.
Reopening the journal reconstructs the verified vote set, so a lost
in-memory response does not turn into fabricated committed progress.

The private `fabric_consumer_progress_commit_observed` gate accepts a
majority of those **persisted observations** and appends a separate
fsynced `Commit` frame. A commit wire message carries the metadata digest,
epoch/leader, sequence, and voter IDs. Followers accept only an exact
locally pending proposal from their installed leader, with structurally
valid membership, and fsync the commit. Duplicate exact updates are
idempotent. A persisted leader decision can be redriven when a commit
message is dropped, including after reopening the same leader's disk.

### Follower COMMIT confirmation and bounded redrive (experimental)

A follower now sends a distinct `COMMIT_ACK` application message **after**
its matching metadata `Commit` frame is fsynced. The current leader binds
the response to the existing transport's peer identity, installed replication
membership and epoch, metadata index, and exact proposal digest, then fsyncs
a separate `CommitReceipt` journal event. Duplicate valid receipts are
idempotent; a false claim, nonmember or changed digest is rejected.

Journal reopen rebuilds each committed decision's follower-receipt set.
`confirmed_commit_sequence` advances only over a **contiguous** history of
decisions whose post-COMMIT receipt sets contain an installed-policy majority.
A missing acknowledgement for decision 1 prevents decision 2 from appearing
confirmed even when 2 has its own majority. A dropped COMMIT receipt is
recoverable by resending the exact same COMMIT, which is idempotent on the
follower and produces a fresh receipt. Bounded leader-only
`redrive_from(stream, replica, start_sequence, limit)` can resend at most
256 stored decisions in historical order; the receiver rejects decisions
without their exact locally persisted `Prepare` predecessor. Gaps or
transport reordering require retry, not implicit skipping.

**These are still private metadata APIs.** A leader's locally confirmed
frontier is *not* independently recovered or quorum-elected by a successor.
The current code does not collect old-policy frontiers across replicas,
recover unresolved COMMIT/Prepare intents, establish cross-epoch quorum
certificates, fence a partitioned old leader with a live grant, or implement
automatic sequential metadata catch-up. No public ACK/NACK/lease cursor
uses this journal, and no client receives a cluster-durable success from it.
A follower's pending metadata does not automatically become committed
when an old leader crashes. A commit on the old leader may still be lost to
the surviving quorum; the replacement must fail closed until recovery is
designed and verified.

The transport checks identities against `incoming.from_node` under
the **existing transport trust model**, which might not cryptographically
authenticate peers. The vote ledger therefore records checked sender
claims, **not unforgeable quorum certificates**. The wire hash is
an integrity/correlation digest, not a signature. Transport ACKs are
never treated as application fsync, metadata commit, or post-COMMIT fsync
acknowledgements.

Deterministic tests exercise RF=2 follower fsync, dropped prepares and
commits, recovered vote sets, majority gating, exact digest matching,
spoofed sender/wrong epoch rejection, and idempotent commit replay.
No Rust tests have been executed locally in the development environment.

### Implemented storage-only foundation

`src/runtime/fabric_consumer_progress.rs` is a private, append-only
`consumer_progress.log` journal with versioned, BLAKE3 hash-chained frames.
`Prepare` fsyncs an exact metadata record but does **not** advance the
committed cursor. `Commit` validates the membership/epoch structure of a
majority acknowledgement certificate and fsyncs a separate commit frame.
Restart replays the full journal and reconstructs the pending proposal,
committed cursor, and acknowledged gaps. Incomplete/corrupt records fail
closed, and uncertain local writes poison that journal instance until reopen.

The journal enforces metadata predecessor continuity, monotonic cursor
updates, bounded and sorted ACK gaps, unique policy members, and majority
certificate cardinality. It explicitly rejects policy/epoch transitions
until a proper old-quorum recovery protocol exists. Unit tests cover RF=2/3
majorities, restart recovery, pending-versus-committed visibility, corruption,
epoch/policy rejection, partition-scoped journal isolation, and ACK-gap preservation.

**This is not an authenticated quorum certificate or distributed durable
consumer protocol.** The private prepare/receipt/commit path now persists
a leader-local quorum decision and propagates it best-effort to followers,
but cannot attest cryptographic sender identity or prove that the commit
decision itself survives failover. It is not wired to
`fabric_stream_ack_consumer_fenced`; existing customer-visible ACKs
remain node-local. The hashes detect accidental corruption but are not a
signature or protection against an attacker who can rewrite the journal.
No safe client-facing cluster-ACK success response has been introduced.

## Product-level contract

- Record replication is separate from consumer progress replication. A
  quorum-committed record is eligible for delivery; it is **not evidence that
  its ACK has been replicated**.
- An ACK must not be reported as *durable across failover* until its consumer
  state is fsynced by a quorum **and** its commit decision is durably recorded.
  On timeout or uncertain commit, the API returns an **indeterminate** status,
  not a fabricated success.
- A promoted leader must reconstruct the highest quorum-proven committed
  cursor and acknowledged gaps before serving its consumers. If it cannot
  prove the committed prefix, fail closed rather than silently creating a new
  cursor at zero and claiming durable consumer continuity.
- Sequence-only ACKs are insufficient for cluster-wide ownership. Validate
  `(stream, partition, consumer, leader_epoch, consumer_generation,
  delivery_attempt_token, sequence)` against persisted lease state.
- A demoted/partitioned leader cannot acknowledge new work after a higher
  epoch becomes quorum-installed. **Local epoch checks are necessary but
  insufficient**: old leaders require a live quorum admission/lease or an
  equivalent consensus grant before claiming success.
- Existing standalone streams retain their local-only durability contract.
  Existing low-level `FileFabricStreamStore` APIs remain storage primitives,
  not cluster-safe consumer APIs.

## Proposed metadata protocol

Maintain a **separate append-only, quorum-replicated consumer metadata log**
per physical Fabric stream partition. Do not encode internal control records
as user-visible application records. Metadata entries include:

```text
ConsumerProgressRecord {
  stream, partition, consumer,
  leader_epoch, consumer_generation,
  metadata_sequence,
  committed_cursor, acked_gaps,
  delivery_attempt_fences,
  previous_metadata_sequence,
  checksum
}
```

The initial implementation can persist a contiguous cursor and bounded ACK
gaps; a compacted snapshot may follow once recovery is proved. All writes
require an installed policy and the applicable epoch promise checks.

### ACK admission

1. Validate the delivery receipt and epoch/generation/attempt fencing.
2. Persist a pending metadata update locally; dispatch identical bytes and
   its predecessor sequence to replica members.
3. Replicas validate policy/epoch/predecessor and **fsync before returning**
   application ACKs. Transport enqueue is not an ACK.
4. After an installed-policy quorum confirms the update, persist the durable
   committed metadata index and disseminate the commit decision. Return
   `Committed` only then; retain a recoverable pending ticket otherwise.
5. On restart, recover pending metadata intents, retry idempotently, and
   never invent success for a non-quorum write.

The existing synchronous `io::Result<()>` ACK surface cannot represent
asynchronous quorum/indeterminate outcomes by itself. Introduce a ticketed
operation with `Pending | Committed | Rejected | Indeterminate` and an
explicit status/poll/await boundary, rather than blocking arbitrary network
progress inside the synchronous Rust store.

### Epoch transition / replacement leader

- The prospective leader obtains quorum-confirmed metadata frontier
  information from the **old installed policy**.
- Select the highest safely committed contiguous metadata prefix; validate
  checksums, predecessors, and consumer-generation identities.
- Catch up from a replica if needed before admitting new consumer deliveries.
  Do not choose a cursor solely from the new leader's local `cursors.json`.
- Fence old metadata epochs and delivery generations durably before returning
  success for new-epoch ACKs.
- Old leaders that lack a current quorum cannot continue to claim
  cluster-durable ACK success, even if their local epoch file is stale.
- For an unconfirmed/lost old quorum, fail closed under the chosen consistency
  policy, and document the resulting availability trade-off.

## Required deterministic regressions

| Scenario | Expected result |
|---|---|
| ACK appended locally, remote ACKs dropped, leader crashes | No durable-success response; safe replay after promotion |
| ACK quorum fsynced and commit persisted, leader crashes | Promoted leader retains cursor; no replay of already committed ACK |
| ACK quorum achieved but reply lost | Client sees uncertain outcome and can retry idempotently |
| RF=3 leader fails during metadata catch-up | Recovery preserves highest quorum-proven metadata prefix |
| Old leader isolated; new epoch installed by majority | Old leader cannot report a new durable ACK |
| Stale delivery attempt ACK after redelivery | Attempt token rejected |
| Same ACK retried before/after restart | Idempotent; no cursor rollback or double advancement |
| Out-of-order ACK gaps across compaction/restart | Contiguous cursor advances only when the missing prefix is ACKed |
| One replica corrupt/torn metadata tail | Fail closed or repair from quorum-proven replica |
| RF=2 loses either node during ACK quorum | ACK cannot claim cluster-durable success |
| Standalone store without replication policy | Existing local lease, ACK, and cursor semantics preserved |

### Benchmark gates

Measure ACK admission rate, fsync volume, p50/p95/p99 quorum ACK latency,
metadata-log growth, redelivery after restart, and catch-up time for 1/10/100k
consumers. Compare both batched and per-record sync policies, report the
durability/fsync policy with each result, and do not compare with JetStream on
unmatched persistence or ACK consistency guarantees.

## Non-goals in draft #1432

Draft #1432 provides local installed-leader checks, epoch-bearing delivery
receipts, and epoch-fenced runtime mutations. It **does not implement** this
replicated metadata journal, quorum ACK tickets, automatic abrupt-failure
removal, live-quorum old-leader fencing, delivery-attempt fencing, or
JetStream-equivalent durable consumer continuity.
