# Fabric replicated consumer progress: safety contract and implementation plan

**Status:** Design proposal — **not implemented**. The current runtime only
persists `deliveries.json`, ACK gaps, and `cursors.json` on the node that
handles the consumer. The leader-epoch APIs in draft #1432 validate the locally
installed epoch, but **do not prove live quorum or replicate consumer progress**.

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
