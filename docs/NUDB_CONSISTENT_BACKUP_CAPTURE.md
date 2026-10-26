# NuDB consistent backup capture contract (design; not yet implemented)

> **Status:** reviewed implementation contract, NOT a shipped backup API, online snapshot guarantee, or permission to back up production data. Applies to the single-node **NUDBWAL3** WAL plus **NUDBCKP1** checkpoint prototype. This document supplements [the NuDB storage promotion runbook](https://github.com/nulang-org/nulang/pull/2202) and the read-only checker/restore test chain [#2206](https://github.com/nulang-org/nulang/pull/2206) → [#2207](https://github.com/nulang-org/nulang/pull/2207).

## Why a clean checksum is not enough

`WalBackedTablet::verify_checkpoint_wal_read_only` validates WAL structural integrity and checkpoint/WAL compatibility **as it reads them**. It does **not** stop another writer from advancing the WAL between checkpoint and WAL reads, and it does **not** prove that overlapping WAL history and a previously published checkpoint have identical contents. Ordinary concurrent copies of `.checkpoint` and `.wal` cannot be advertised as a consistent backup.

A backup requires an explicit, stable **source cut**, a durable **capture publication** boundary, and a **disposable restore** that verifies the intended committed state. Until all three hold, output is an *unverified export*, not a recoverable backup.

## Supported first implementation: owner-coordinated, single-tablet capture

This protocol is scoped to a *trusted local filesystem* with one cooperating NuDB WAL writer, not NFS, object-storage eventual consistency, multiple primaries, or a distributed tablet.

1. **Enter from the live owner, not a second inspector.** Proposed narrow API: `WalBackedTablet::capture_consistent_backup(&self, destination) -> Result<BackupReceipt, BackupError>`. The owned `FileWal` maintains its exclusive sidecar writer lock across the entire synchronous capture. A shared borrow of `WalBackedTablet` prevents the owning Rust instance from mutating its tablet/WAL during the copy. Other authorized local writers must honor the same lock; the filesystem/path must be trusted. Reject a poisoned or unreconciled writer before copying. On-disk repairs are not allowed in the source capture path.
2. **Fix the source sequence and descriptor.** At the start record current sequence, tablet ID, ownership epoch, key range, WAL base/tail, and checkpoint sequence if present. Accept all three supported layouts: uncheckpointed WAL base 0; published checkpoint with unreclaimed WAL; compacted WAL with checkpoint at its base. Reject missing required checkpoints, invalid or stale descriptor/sequence/fencing, corrupt complete frames, torn source tails, and sequence rollback. A read-only verifier must run under the owner-held quiescence boundary.
3. **Create a fresh staging area, fail closed.** Staging and final output are siblings on the same filesystem beneath a trusted caller-owned backup root. Never overwrite a preexisting backup directory, follow destination symlinks, or rely on a predictable shared temp path. Use `create_new` for files; make destination directory ownership/access private. Avoid hardlinks to live WAL/checkpoint—those would not be frozen copies. Do not rely on `fs::copy` if destination creation can follow untrusted links.
4. **Capture actual bytes while the owner stays quiescent.** Copy a bounded buffer in a streaming loop, from the open source files into **new** destination files; copy the complete WAL and checkpoint if present. Sync both destination files. Validate measured sizes and BLAKE3 hashes. Read/parse from the *staged* copies and rerun read-only WAL/checkpoint-pair checks. Recheck the owner's sequence, epoch, and source file identity/length to detect unexpected mutation, and abort if any changed. A snapshot must not expose half-written backup files as complete.
5. **Prove recoverability without touching the source.** Open a *separate disposable copy* of the staged pair with the ordinary recovery implementation (which may legitimately truncate a torn tail **only in that disposable directory**). Require no repair/truncation when the staged structural verifier passed. Compare recovered tail sequence, descriptor, representative or exhaustively indexed historical versions, tombstones, and durable idempotency receipts against the owner's state at the source cut. The first implementation should use an independent deterministic reference-model digest of logical state; merely reopening successfully is insufficient for a correctness claim.
6. **Durably publish completion.** Write a versioned manifest last, including backup ID, tablet descriptor, ownership epoch, WAL base/tail and checkpoint sequence, file sizes, per-file BLAKE3 digests, and verification result. No values, credentials, or tokens in manifest/logs. Sync manifest and staging directory. Perform a documented no-replace atomic publication operation into the final destination, then sync the parent directory. If the platform cannot guarantee no-replace publication, do not advertise that path as atomic. Return `BackupReceipt` only after this barrier and successful staged restore proof. Missing completion marker means *incomplete*, never usable.
7. **Failure handling:** on source mutation, disk-full, read/write/short-copy, checksum, rename/sync, conflicting destination or restore mismatch: **fail closed, do not return a receipt, do not alter source**, and preserve an identifiable non-authoritative staging artifact or safely clean only the staging directory created by this operation. Never recursively delete user-supplied destination roots. A retry gets a fresh staging ID; never append to a partially published backup.

### Capture invariants

| Property | Required proof |
| --- | --- |
| No committed data loss | Durable sequence at capture equals recovered sequence |
| No torn snapshot | Same owner-held writer lock and in-process quiescence for checkpoint+WAL byte copy |
| Complete data/receipt replay | Independent state/model comparison across put, delete, repeat-key, retry/receipt cases |
| No WAL repair during verification | Source and staged checks run read-only; crash-tail truncation confined to disposable restore copy |
| No accidental source modification | Compare original file metadata/digests before and after controlled fault injections |
| Bounded memory and I/O | Streamed copy, bounded verifier decoder, incremental BLAKE3; no whole-WAL buffering |
| No partial backup publication | Manifest last, safe no-replace publication, sync file + directory + parent |
| No destructive overwrite | Existing destination/root and source files remain byte-for-byte untouched |

## Required first acceptance suite (test-first)

- **RED**: a multi-transaction workload (including tombstones, repeated-key last-write-wins, idempotent retries, mixed historical reads) cannot produce a verified backup receipt until the owner-coordinated API exists.
- **GREEN source layouts**: WAL-only, checkpoint+full WAL, checkpoint+reclaimed WAL, checkpoint then additional WAL suffix; exact tail and receipt parity on recovered disposable copies.
- **Races**: second writer is rejected while capture holds the lock; capture blocks/serializes the same-instance writer; opening only the read-only inspector while writer mutates is explicitly *not* an accepted online-backup protocol.
- **Corruption/failures**: oversized frame, complete corrupt checksum, semantic receipt digest mismatch despite valid frame checksum, incomplete final header/payload/checksum, corrupt checkpoint, missing checkpoint with reclaimed WAL, old/ahead checkpoint, stale epoch, invalid ownership descriptor and wrong tablet ID. Every case fails without truncating or deleting source evidence.
- **Publication failpoints**: before/after WAL stage write, stage fsync, checkpoint stage write, manifest write/sync, directory sync, publication rename, parent sync; no failure may leave a misleading valid completion receipt.
- **Security boundary**: refuse existing target, traversal, destination symlinks, cross-filesystem rename, hardlink aliasing, and permissions exposing data to other local users. No secret values in audit output.
- **Resource envelope**: check disk headroom up front, copy with fixed-size buffers, measure MB/s, peak RSS, pause duration for writer, and tail-writer backpressure. Require clear operator abort/retry procedure when storage is full.

## Explicit exclusions and later phases

- **Do not retrofit `FileWal::open` as a verifier**: it can create/repair/truncate source evidence.
- **Do not snapshot by independently copying active files**: coherent file-pair reading is not guaranteed across owner changes or WAL reclaim/rename.
- **Do not claim distributed leases or multi-node consistency**: sidecar locks are single-host cooperative exclusion only.
- **Do not implement incremental backup or remote R2/S3 publication as part of this initial local proof**. Remote backup requires separately verified access control, encryption, durable object identity/checksums, multi-object publication manifest, retention and an independently demonstrated restore. No public object URLs should be used for private backup data.
- **Do not automatically migrate production persistence** from SQLite/libSQL/PostgreSQL to NuDB based on passing prototype backup tests.

## Order of implementation

1. Reconcile and validate the exact source tree through NuDB receipts (#2204), read-only WAL (#2206), checkpoint pairing (#2207), and MVCC snapshot search (#2205 or its compatible port); keep the security ancestry #2184 → #2199 → #2201.
2. Introduce explicit owner-held, non-mutating snapshot cut and private staging API with failpoints; require source audit and durable staged-copy verification.
3. Add independent deterministic state/receipt oracle and disposable restart-proof that rejects even checksum-valid but semantically mismatched checkpoints.
4. Add no-overwrite atomic publication + manifest, crash/disk-full tests, integrity checksum, and measured pause/throughput envelope.
5. Only then document a **supported consistent local single-tablet backup**. A reliable online backup requires measured acceptable writer pause time and separately reviewed deployment/restore policy.

**Merge restriction:** this document is an engineering contract, not an implementation; keep related code PRs draft until exact-head hosted status and independent security/storage review pass. No gate bypass, source mutations, or production backup exports authorized by this document.
