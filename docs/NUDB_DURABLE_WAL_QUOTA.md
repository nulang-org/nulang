# NuDB durable WAL quota — opt-in local owner admission

**Draft feature, not a production storage quota.** This protocol provides an immutable
physical-byte ceiling for a single canonical `FileWal` and one tablet ownership
epoch. It does **not** provide disk reservations, distributed coordination,
automatic checkpoint/reclamation, MVCC garbage collection, or remote snapshots.

## Activation and durability ordering

1. Open `WalBackedTablet` normally. The canonical local WAL writer lock is held.
2. Call `activate_durable_wal_quota(max_bytes)` once for the tablet's current
   ID and ownership epoch. `0` is valid and denies all new WAL frames.
3. A unique `<wal>.quota` sidecar is created using `create_new`. It stores
   magic `NUDBQOT1`, tablet ID, ownership epoch, unsigned 64-bit limit, and a
   BLAKE3 checksum. It is fully written and synced, then its parent directory
   is synced **before** activation reports success.
4. All single-frame and group WAL append paths, including the legacy and
   explicit-budget APIs, check their *complete projected encoded WAL file
   length* before the first append. The effective limit is the smaller of
   the durable limit and any per-call budget. Idempotent receipt replays that
   do not append new WAL frames remain valid.
5. The quota sidecar survives checkpoint publication, verified WAL replacement
   and restart. A torn, corrupt or aliased quota prevents writable WAL open;
   it is **never** interpreted as an absent/unlimited configuration.

Quota activation can be repeated only with **exactly the same** ID, epoch and
limit. Raising, lowering, removing or transferring the quota is not available.
An ownership-epoch change intentionally fails closed until a separately
reviewed, authenticated durable transition protocol exists.

## Failure handling

A write failure after the quota sidecar is created poisons the live WAL
handle; retry by reopening and inspecting the sidecar. Do not unlink, reset,
overwrite or rename a stranded or corrupt sidecar just to restore writes.
A partially installed quota fails closed on the next open. A missing WAL with
an otherwise valid quota sidecar also fails closed before file creation, even
when a checkpoint remains: recovery of lost WAL storage requires an explicit
operator-controlled procedure, not implicit creation of an empty log.
The existing WAL data is not rewritten by quota activation.

A budget rejection before any frame mutation is retryable and preserves
MVCC state, receipts and retention pins. Ordinary I/O failures **after** WAL
frame writing retain the existing ambiguous-prefix/reopen-and-reconcile
contract. No automatic consumer release is attempted to make space.

## Explicit trust boundaries

- **Trusted single-host local filesystem** with writer exclusion. A privileged
  external actor deleting/replacing the sidecar or WAL path is not prevented
  cryptographically by this unkeyed checksum; the directory must be under
  protected operator control. Network FS, multi-process malicious racing
  path replacement, distributed fencing and replication are unsupported.
- This protects **WAL file bytes only**, not the size of checkpoints,
  temporary files, backups, metadata, unrelated tablets or free disk space.
  Compaction can reduce WAL bytes but cannot violate retention safety.
- The quota is not a tenant-wide billing limit; it does not guarantee fair
  admission or prevent ENOSPC. No policy for quota rotation or emergency
  operator recovery is supplied in this draft.
- Full power-fail certification, all-target validation, release builds,
  independent security review and parent PR approvals remain mandatory
  before operational adoption.

Focused coverage: `tests/nudb_durable_wal_quota.rs` and
`tests/nudb_wal_budget.rs`. Test using `--locked --no-default-features`,
then required hosted CI and storage/recovery matrices on the exact merged
tree.
