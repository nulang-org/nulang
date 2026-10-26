# NulangDB storage-engine implementation order

This document fixes the near-term sequencing for the NulangDB storage kernel.
The objective is to make the single-node engine measurably strong before
adding replication or distributed transactions.

## Phase 1 — write path and measurement

1. Binary batched WAL with one durability sync per batch.
2. Criterion coverage for 1/16/64/256-write batches.
3. Preserve the existing crash-safe `FileWal` as the correctness baseline until
   the batch path has equivalent interruption and hard-kill coverage.

## Phase 2 — persistent local storage

4. Mutable memtable.
5. Immutable memtable rotation.
6. SSTable v1 with checksummed blocks, block index, and bloom filter.
7. Manifest with active SST inventory and stable sequence.
8. Background flush while the serving memtable continues accepting writes.
9. Leveled compaction and MVCC version/tombstone GC.

## Phase 3 — multicore execution

10. Pin each mutable tablet to exactly one database shard/core.
11. Keep same-shard execution direct and lock-free on the normal path.
12. Use bounded typed queues only for cross-shard handoff.
13. Move tablets between shards through epoch-fenced drain/handoff.

## Phase 4 — query execution

14. Vectorized `DataBatch` abstraction.
15. Batch scan/filter/project operators.
16. Data-skipping metadata on immutable blocks.
17. Add columnar analytical projections only after the row-oriented storage path
    is competitive and stable.

## Phase 5 — distribution

18. Replicate commit batches rather than individual logical writes.
19. Range placement and migration.
20. Cross-tablet transaction experiments after the single-tablet engine is
    benchmarked and crash hardened.

## Explicit non-goals for the current slice

- Raft/Multi-Raft
- distributed 2PC
- SQL/PG wire changes
- HNSW/vector indexing
- time-series special storage
- io_uring/direct-I/O tuning

Those optimizations depend on a measured local storage baseline and should not
precede it.

## Group-commit convergence gate (do not ship parallel WAL semantics)

The batch storage and durable transaction-receipt drafts solve different
problems and currently diverge. They must be reconciled **before either becomes
the public serving API**:

| Contract | Batched-storage integration (#2192) | Receipt-based draft (#2177) |
| --- | --- | --- |
| Per-request acknowledgment | Committed sequence; `TabletRequestId` is process-local | Persistent `TransactionId` + digest-bound `TransactionReceipt` |
| Group admission | Owner: 256 writes / 1 MiB / 100 µs defaults | Identified requests: at most 64 records / 8 MiB |
| Lower-level WAL ceiling | Direct `FileWal::append_batch`: 4,096 frames / 64 MiB | 64 frames / 8 MiB |
| WAL publication | Checksummed individual `NUDBWAL3` frames, one sync, MVCC after sync | Same required frame/replay behavior plus durable receipt deduplication |
| Interrupted batch | Poison live writer; close, reopen, replay valid prefix | Also retry the **same** transaction IDs and mutations to reconcile receipts |
| Recovery | Sequential replay; no all-or-nothing batch guarantee | Same; receipt IDs and digests must also survive checkpoint/restart |

**Decision for this stage:** Keep `NUDBWAL3` `FileWal` as the sole serving
durability format. Treat `BinaryBatchWal` as experimental; do not switch
serving writes to `NUDBBW01` until a separate audited migration and crash
contract exists. Preserve the distinction between **scheduling budgets**
(owner), **storage hard ceilings** (`FileWal`), and **API limits** (durable
transaction IDs). The owner request counter must never be presented as a
durable idempotency key.

**Promotion sequence:**

1. Certify #2192's fenced writer, owner admission, replay, checkpoint,
   interruption, and byte-format contracts at the same exact commit.
2. Select one `append_batch` implementation. Integrate #2177's stable
   transaction identity, hash/conflict check, receipt persistence and mixed
   replay/new-write ordering **above** that shared WAL primitive; do not
   introduce a second independent frame writer or duplicate poisoning logic.
3. For a failed or ambiguous group, persist no premature MVCC state. After
   reopening, allow retries with the same durable IDs, preserving the recovered
   prefix and rejecting changed payload digests. Never promise batch atomicity.
4. Gate a public idempotent batch API on explicit tests for duplicate IDs,
   conflicting retries, retries after torn/checksummed frames, prior checkpoint,
   ownership-epoch fencing, stale-writer exclusion, and preserved receipt order.
5. Run `cargo fmt --all -- --check`, the exact-head storage suite,
   `cargo test --locked --no-default-features --lib database::interruption::tests`,
   default and WASM builds, CodeQL, and **unmodified `cargo audit`** against
   an independently reviewed security dependency baseline. A TLS handshake
   unit test does not substitute for authorized, authenticated remote-libSQL
   database operations. Do not merge before those gates pass.

The overlapping `store.rs`, `wal.rs`, `interruption.rs` and
`nudb_group_commit.rs` implementations in #2177 and #2192 are intentionally
**not** automatically combined. Reconcile them in one draft, remove redundant
paths, then independently validate the result. This avoids two interpretations
of durability and prevents maintenance entropy as the engine grows.
