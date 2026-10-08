# NuDB: Single-node durable tablet split cutover

Status: experimental, **single-node** prototype with advisory, exclusive
process-level ownership. This is a storage correctness milestone, not a
distributed database protocol.

## Contract

A tablet initially owns the half-open interval `[start, end)` with one
per-tablet WAL and a checksummed routing manifest. Under the coordinator's
exclusive ownership of the directory, the split operation:

1. Materializes both child MVCC histories from a stable committed source
   sequence `S`. Both children begin with sequence `S` and a strictly newer
   ownership epoch.
2. Publishes/fsyncs a child checkpoint for each exact key range.
3. Creates/fsyncs a **new** child WAL (never overwrites another WAL) with a
   checksummed header recording the child ID, epoch, and `base_sequence = S`.
4. Reopens both children, proving that checkpoint and WAL agree.
5. Marks the live coordinator **poisoned**, then writes/fsyncs a temporary
   routing manifest and renames it over the old manifest. On Unix, syncs its
   parent directory to complete durable publication.
6. Only after confirmed publication does the coordinator begin dispatching
   writes by the child range boundary.

The routing manifest is the sole authoritative cutover decision:

| Manifest state | Recovery behavior |
| --- | --- |
| Parent | Open parent; ignore any staged child artifacts |
| Split | Require and open **both** children; never fall back to parent |
| Corrupt/missing with tablet files | Fail closed |
| Publication result ambiguous | Poison live coordinator; reopen from disk |

Before the manifest rename, staged child bytes are **not live state**. A retry
under exclusive directory ownership discards abandoned staging and derives
fresh children from the current committed parent.

After publication, separate child WAL sequences can advance independently.
The inherited sequence is not a global distributed transaction timestamp.
Cross-tablet snapshot or atomic transaction semantics are **not provided**.

## Crash simulation coverage

- After left child checkpoint/WAL staging: parent remains authoritative
- After right child staging: parent remains authoritative
- After temporary routing manifest fsync, before rename: parent remains authoritative
- After routing manifest rename: in-process handle refuses stale reads/writes;
  reopen chooses the published children
- Corrupt manifest: fail closed
- Missing child checkpoint or WAL: fail closed, even at source sequence zero
- Truncated parent or child WAL: fail closed before WAL auto-initialization
- Child writes after promotion/restart preserve their independent sequence tails
- A second `SingleNodeSplitStore` opener (same process or another process) is
  refused while the first is alive, including after split publication
- The stable lock inode persists after the owner drops so later processes
  cannot accidentally acquire distinct locks on different inodes
- Public WAL and tablet opens are refused for managed directories even after
  owner exit and child publication; old public handles cannot append, reclaim,
  or checkpoint after the persistent owner marker appears

The suite includes both injected in-process interruptions and a subprocess
that terminates via `std::process::exit(72)` at each publication boundary,
without running destructors. The parent process reopens the directory and
verifies the authoritative manifest, values, and released advisory lock.

These tests cover real process termination but **not sudden power loss**,
filesystem reorderings, or storage hardware faults.

## Explicit exclusions and next gates

- **Advisory single-node process coordination:** `SingleNodeSplitStore::open`
  acquires a nonblocking exclusive OS lock on the stable `.nudb-owner.lock`
  inode **before** reading or mutating tablet storage; the `File` remains
  owned until the store is dropped. An independent coordinator opening the
  same root fails with `OwnerBusy`. Never unlink or rotate that file, even
  after crash recovery.
- **Cooperative public WAL API fence:** `FileWal::open` and
  `WalBackedTablet::open` fail closed when a persistent
  `.nudb-owner.lock` or `route.manifest` is present. Only the crate-private
  managed-open path, scoped to a held `OwnedDirectory` lock and the matching
  canonical root, can recover a managed tablet. Previously opened public WAL
  handles recheck before append/reclamation, and public tablet handles recheck
  before checkpoint publication.
- **Advisory, not physical fencing:** An uncooperative process can still edit
  underlying files directly; a public handle that passed its admission check
  before a concurrent owner transition has a possible TOCTOU window.
  Do not use this as distributed ownership fencing or a protection against
  malicious filesystem writes. Multi-process migration requires a protocol
  preventing takeover until old owners have stopped, with a durable
  monotonically increasing fencing token validated at commit time.
- **No distributed fencing:** Add lease/consensus-backed ownership and durable
  tablet placement metadata before node migration or shared-storage failover.
- **No global SQL transaction ordering:** Cross-tablet snapshots, two-phase
  transactions, and distributed atomic writes require a separate protocol.
- **No physical crash validation:** Run fail-stop/power-loss tests using a
  controlled filesystem harness and record fsync semantics across platforms.
- **No retained WAL cleanup for the parent:** GC should only reclaim retired
  tablet bytes after verified publication, pins, and backup retention checks.
- **No Arrow scan path yet:** Integrate the existing snapshot-consistent row
  iterator with Arrow batches only after storage cutover is validated.

## Validation

```sh
cargo fmt --all -- --check
cargo test --no-default-features --test nudb_managed_wal_guard
cargo test --no-default-features --test nudb_exclusive_owner
cargo test --no-default-features --test nudb_split_cutover
cargo test --no-default-features --test nudb_snapshot_scan_split
cargo test --no-default-features --lib database::split
cargo test --no-default-features --test wal_backed_tablet --test checkpoint_reclamation --test tablet_wal
```
