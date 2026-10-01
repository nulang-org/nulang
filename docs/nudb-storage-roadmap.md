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
