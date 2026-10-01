# NulangDB mutable and immutable memtables

This slice introduces an in-memory generation boundary without changing NulangDB checkpoint or WAL formats. `MemoryTablet` owns one mutable memtable plus frozen immutable generations ordered oldest to newest.

Writes and WAL replay append MVCC versions to the mutable generation. Rotation is explicit and byte-target driven; empty generations are never emitted. The byte count is logical estimated storage size for rotation policy, not allocator accounting.

Reads search the mutable generation first and then immutable generations newest to oldest while respecting the requested MVCC sequence. Tombstones therefore hide older values at new snapshots without destroying historical reads.

Checkpoint serialization flattens immutable generations followed by the mutable generation back into the existing `TabletSnapshotState` schema. Checkpoint recovery restores that flattened state as one immutable baseline and leaves the mutable generation empty, so subsequent WAL replay and commits remain a newer mutable generation.

`WalBackedTablet` exposes rotation and generation metrics as management hooks only. Commit durability ordering remains `validate -> WAL sync -> publish MVCC`; rotation does not reclaim WAL data and does not create an additional persistence boundary.
