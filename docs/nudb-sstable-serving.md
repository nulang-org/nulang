# NuDB SSTable serving

Manifest-validated `NUDBSST1` files are an active immutable MVCC read tier. The physical v1 file format remains unchanged while the serving path now uses a row-aligned block index and a bounded raw-block cache.

## Serving model

- The mutable memtable and unflushed immutable memtables remain resident and serve the newest local versions.
- A successful immutable flush writes and syncs the SSTable, publishes the manifest entry, installs the validated SSTable in the serving set, and only then retires the duplicate in-memory generation.
- Reopen validates the complete SSTable payload checksum and manifest identity, but does not retain decoded row/value bodies.
- During validation, the reader builds logical row-aligned blocks targeting roughly 64 KiB and records each block's key range, file offset, length, and BLAKE3 checksum.
- Snapshot reads choose the newest version at or before the requested sequence across resident and SSTable tiers. Tombstones participate in the same ordering and remain authoritative.
- An SSTable miss reads only the indexed target block, verifies its block checksum, then decodes that block. Post-open mutation/corruption of an uncached block therefore fails closed at the read boundary.
- Every SSTable in one `WalBackedTablet` shares one cache budget. The cache stores verified raw encoded blocks, so `resident_bytes` is an exact payload-byte budget rather than an allocator estimate.
- Cache capacity is configurable with `WalBackedTablet::open_with_sstable_cache_bytes`; a zero-byte budget preserves block-backed reads while disabling cache residency.

## Read error boundary

Block-backed reads introduce a real storage I/O boundary after tablet open. `WalBackedTablet::read_at` and `read_latest` therefore return `Result` instead of treating a disk failure as a missing key.

Hot resident values are returned as `Cow::Borrowed`, so the memtable path remains zero-copy. SSTable values are returned as `Cow::Owned` after the verified block is decoded.

This is intentional: corruption, truncation, permission failures, or other SSTable read errors must remain distinguishable from MVCC absence/tombstones.

## Checkpoint safety

A checkpoint remains a self-contained recovery authority before WAL reclamation. Before publishing a checkpoint, `WalBackedTablet` sequentially scans active SSTables outside the serving cache, merges their MVCC history with resident memtable history, rejects conflicting values for the same key/sequence, reconstructs one canonical snapshot, and writes that snapshot through the existing atomic checkpoint protocol.

```text
SSTable sequential scan + resident MVCC history
    -> compose canonical snapshot
    -> write + sync checkpoint
    -> reclaim WAL through checkpoint sequence
```

Checkpoint scans deliberately bypass the block cache so a maintenance operation cannot evict the hot serving working set. The system also does not require the SSTable manifest to survive after a checkpoint has authorized WAL reclamation; the checkpoint remains complete on its own.

## Integrity model

`NUDBSST1` still carries its original whole-payload checksum. Open validates that checksum while streaming the payload and deriving the in-memory block index. Each logical block also gets an in-memory BLAKE3 digest derived from those already-validated bytes. Later on-demand reads revalidate the target block against that digest.

The block digests are not a new on-disk format and do not weaken compatibility with existing SSTables.

## Deliberate limits

- SSTable writes still materialize the encoded payload in memory before atomic publication; this slice optimizes the read/open residency path, not the flush encoder.
- The block index retains first/last keys and sequence-coverage metadata in memory. It does not yet use prefix compression, bloom filters, or an on-disk index.
- No L0→L1 compaction, obsolete-SSTable deletion, MVCC garbage collection, or new WAL reclamation policy is introduced.
- No distributed replication semantics change.
- This does not replace Nulang Cloud's current per-actor SQLite/libsql DB effect backend. NuDB should earn that role through correctness and comparative workload evidence rather than an architectural rewrite.

## Next slice

Add L0 compaction with crash-safe manifest replacement and explicit obsolete-file retirement. After compaction is proven, add MVCC garbage collection and tie WAL/SSTable reclamation to durable sequence coverage. In parallel, benchmark NuDB against SQLite/libsql on actor-local point reads, write batches, checkpoint/restore, memory footprint, and migration workloads before considering a Nulang Cloud backend change.
