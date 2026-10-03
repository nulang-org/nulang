# NuDB SSTable serving

This slice promotes manifest-validated `NUDBSST1` files from reopen-only recovery inputs into an active immutable MVCC read tier.

## Serving model

- The mutable memtable and unflushed immutable memtables remain resident and serve the newest local versions.
- A successful immutable flush writes and syncs the SSTable, publishes the manifest entry, installs the validated SSTable in the serving set, and only then retires the duplicate in-memory generation.
- Reopen validates manifest metadata and SSTable identity exactly as before, but no longer copies SSTable rows back into `MemoryTablet`.
- Snapshot reads choose the newest version at or before the requested sequence across the resident and SSTable tiers. Tombstones participate in the same ordering and therefore remain authoritative.
- The v1 SSTable reader still decodes each file into process memory. This removes duplicate memtable residency but is not yet block-backed I/O or a bounded block cache.

## Checkpoint safety

A checkpoint remains a self-contained recovery authority before WAL reclamation. Before publishing a checkpoint, `WalBackedTablet` merges the active SSTable MVCC history with resident memtable history, rejects conflicting values for the same key/sequence, reconstructs one canonical snapshot, and writes that snapshot through the existing atomic checkpoint protocol.

This ordering is deliberate:

```text
SSTable + resident MVCC history
    -> compose canonical snapshot
    -> write + sync checkpoint
    -> reclaim WAL through checkpoint sequence
```

The system therefore does not require the SSTable manifest to survive after a checkpoint has authorized WAL reclamation. SSTables remain useful serving/recovery artifacts, but the checkpoint is complete on its own.

## Deliberate limits

- No new WAL reclamation policy is introduced beyond the existing checkpoint-authorized reclamation path.
- No L0 compaction, obsolete-SSTable deletion, MVCC garbage collection, bloom filter, block index, or block cache is added here.
- No distributed replication semantics change.
- This does not replace Nulang Cloud's current per-actor SQLite/libsql DB effect backend. NuDB should earn that role through correctness and comparative workload evidence rather than an architectural rewrite.

## Next slice

Add indexed/block-backed SSTable reads with a bounded cache so large immutable generations no longer require full-file decode on open. After that, introduce compaction and tie obsolete WAL/SSTable reclamation to explicit proven sequence coverage.
