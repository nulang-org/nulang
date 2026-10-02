# NuDB SSTable v1

This slice adds durable immutable flush artifacts without promoting SSTables to recovery authority yet.

- `NUDBSST1` stores sorted MVCC rows in a checksummed binary file.
- `NUDBMAN1` records durable SSTable inventory with temp-write, sync, rename, and directory-sync publication.
- `WalBackedTablet::flush_oldest_immutable_to_sstable` writes the oldest frozen memtable and publishes its manifest entry.
- Flush is content-addressed/idempotent. A repeated flush of the same immutable generation does not duplicate the manifest entry.
- The immutable generation remains resident after flush. WAL/checkpoint recovery remains authoritative and WAL reclamation is unchanged.
- Crash-interruption tests cover SSTable and manifest publication boundaries and verify reopen still reconstructs from WAL.

The next storage slice can promote manifest-backed SSTables into recovery/read serving, then make immutable eviction and WAL reclamation conditional on proven durable coverage.
