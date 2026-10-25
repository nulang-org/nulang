# NuDB storage promotion: canonical safety baseline and acceptance gates

> Status: **review-only runbook**, 2026-10-25. This document does not mark NuDB production-ready, authorize a merge, or change the operational `NUDBWAL3` format. Recheck every PR head, parent, run, and merge status before applying it.

## Decision

Promote **one** validated single-node storage lineage before adopting competing performance/LSM drafts. The operational baseline is the WAL-backed tablet with ownership/sequence fencing, validated-before-fsync publication, crash-tail recovery, fail-closed corrupt frames, and checksummed atomic checkpoints. The next baseline should retain those invariants while adding vetted writer isolation, duplicate-key recovery, bounded group admission, and patched libSQL TLS dependencies.

**Do not independently cherry-pick overlapping group-commit, receipt, or MVCC implementations into `main`.** Do not change storage format during this consolidation. Continue using SQLite/libSQL for production persistence where their guarantees are required.

## Observed starting point (snapshot; revalidate)

- `main`: `448cfa8c2a84588fc1c7c7d94bbb0577ad72e3d0` at the time of review.
- [#2167](https://github.com/nulang-org/nulang/pull/2167) `main`-based single-node writer-fencing and duplicate-key safety draft.
- [#2173](https://github.com/nulang-org/nulang/pull/2173) legacy checkpoint compatibility, based on the #2167 branch.
- [#1209](https://github.com/nulang-org/nulang/pull/1209) bounded canonical JSON WAL batching and separate experimental binary WAL.
- [#1213](https://github.com/nulang-org/nulang/pull/1213) owner-admission batching and backpressure, stacked.
- [#2179](https://github.com/nulang-org/nulang/pull/2179) borrowed WAL serialization.
- [#2192](https://github.com/nulang-org/nulang/pull/2192) integration of #2173 + batching + owner admission + serialization, stacked on #2173.
- [#2184](https://github.com/nulang-org/nulang/pull/2184) RustSec/libSQL TLS remediation, based on `main`.
- [#2199](https://github.com/nulang-org/nulang/pull/2199) **review-only composed integration** of the NuDB source line and #2184, targeting the #2184 branch rather than `main`.
- [#2201](https://github.com/nulang-org/nulang/pull/2201) pre-connect libSQL remote URL/bearer-token admission; child of #2199, not an independent release.
- [#2177](https://github.com/nulang-org/nulang/pull/2177) *separate*, stacked transaction-ID/receipt-aware bounded batching implementation. Its receipt API is **not part of #2199**.

As observed on 2026-10-25 for exact integration head `fae541f956324a482bf308d4a7ca7b2753808ef4`, [NulangDB Storage run 40231629702](https://github.com/nulang-org/nulang/actions/runs/40231629702) and [CI run 40231629758](https://github.com/nulang-org/nulang/actions/runs/40231629758) completed successfully. CI included Build & Test, no-default-features, WASM-feature build, Format/Clippy/Audit, release and package jobs. These are **head-specific observations**, not approval for other heads or assurance of a safe final `main` merge. As observed in the same review, #2201 still had queued CI.

## Promotion order and non-bypassing gates

### Gate A — canonical storage + security composition

1. Record exact heads, base refs and changed paths of #2167, #2173, #1209, #1213, #2179, #2192 and #2184. Confirm #2199's tree contains the intended safe storage source **without** inadvertently importing #2177's separate receipt model. Resolve overlapping files **once** in a reviewed, source-coherent tree.
2. Validate #2184's TLS/RustSec changes independently, including certificate-chain and hostname failures, remote authenticated HTTPS use under authorized test credentials, and an unmodified `cargo audit`. No certificate-verification bypass, credential logging or advisory exceptions to make a gate pass.
3. Treat #2199's green run as a valid signal **only for that immutable SHA**. Obtain independent review of the combined diff and its branch dependency. Since #2199 targets the security branch, do not confuse merging this child PR with promoting the whole composition to `main`.
4. Before `main` promotion, choose one explicitly reviewed strategy: (a) merge the base dependencies in order and rebuild/rebase the integration against the resulting `main`, or (b) construct one fresh `main`-targeting integration PR proving source/tree parity. Run required checks on the **new final SHA**, not merely on its old parents. Do not double-merge ancestors or independently merge the conflicting alternate #2177 line.
5. After Gate A, replay #2201 onto the **actual promoted parent**; prove URL admission happens before token use, DNS, socket creation, and runtime initialization. Re-run its public API and TLS tests at the new exact head before separate promotion. Do not expose live bearer tokens in CI output.
6. Keep branch protection and required hosted statuses binding. Never use a local green run, skipped benchmark lane, or status from a parent commit to override a missing/failing merge check.

### Gate B — single-node durability and recovery (must hold on Gate A candidate)

- **Write ordering:** validate all constraints before first WAL mutation; acknowledge only after durable sync; make committed data visible after sync. An append/sync failure poisons the writer until reopen/reconciliation.
- **Group semantics:** a group of independently framed transactions can leave a fully formed committed **prefix** after an ambiguous crash; it is *not* an atomic multi-transaction batch. No client ACK before whole-group sync. Stable transaction IDs and receipts, where present, must reconcile replay and retries without duplicate effects.
- **Writer exclusivity:** a local lock guards creation, repair, append and WAL path replacement, including checkpoint/reclaim. Do not infer distributed consensus or correctness on unreliable network filesystems from an OS lock.
- **Fencing:** reject stale tablet IDs, ownership epochs, predecessor sequences and out-of-range mutations before publication. Historical WAL replay must preserve legal past ownership epochs while rejecting broken sequence chains.
- **Duplicate keys:** multiple writes to the same key in one transaction yield exactly one final version at that sequence; older checksum-valid duplicate-version checkpoints must have explicit, tested compatibility handling.
- **Recovery:** truncate *only incomplete final crash frames*; reject complete checksum-invalid frames, malformed record lengths, invalid manifests, incompatible checkpoint state, and checkpoints ahead of WAL. Do not silently treat storage corruption as a missing key.
- **Checkpoint:** durable temp write -> sync -> atomic rename -> parent-dir sync -> optional WAL reclaim. Recovery must accept checkpoint + unreclaimed WAL, and checkpoint + matching compacted WAL. Preserve fail-closed handling of errors around publication/reclamation.
- **Interruption matrix:** crash/kill at WAL header, payload, checksum, sync, checkpoint temp write/sync/rename/directory sync, and reclaim replacement write/sync/rename/directory sync. Verify final state against a deterministic model, including restart and repeated-key writes.
- **Adversarial tests:** size/length/count bounds before allocation; checksums covering frame metadata and payload; disk-full / short-write / corrupt committed frame; locked second writer; no unchecked allocations from corrupted input.

Focused candidate commands (run against the candidate's pinned `Cargo.lock`, correcting test names **only when the candidate's workflow defines them**):

```bash
cargo +1.95.0 fmt --all -- --check
cargo +1.95.0 test --locked --no-default-features --test nudb_batch_safety --test nudb_group_commit --test nudb_single_node_safety --test nudb_legacy_checkpoint --test nudb_crash_recovery --test checkpoint_reclamation --test tablet_owner_admission
cargo +1.95.0 test --locked --no-default-features --lib database::
cargo +1.95.0 test --locked --no-default-features --features sqlite --lib runtime::libsql_tls::tests
cargo +1.95.0 test --locked --no-default-features --features sqlite --test tls_dependency_security
cargo +1.95.0 audit
```

The exact-head GitHub `NulangDB Storage`, `CI`, security, required release and branch-protection checks still control merge eligibility. A command listed here is **not** a claim that it passed.

### Gate C — receipts and retention: reconcile, do not duplicate

- #2177 is a **separate** transaction-ID and receipt design stacked on #1736 -> #1739 -> #1742. It is not automatically compatible with #2199's batched canonical WAL. Once Gate A lands, compare idempotency data models and failure semantics; port a single coherent receipt contract and run interruption/replay coverage. Bounded receipt retention must never cause unsafe retry ambiguity.
- [#1702](https://github.com/nulang-org/nulang/pull/1702) and [#1289](https://github.com/nulang-org/nulang/pull/1289) contain different MVCC reclamation implementations. Select one correct safe-horizon mechanism. GC cannot advance past active snapshot pins, backup snapshots, or replica/changefeed retention requirements. Persist the oldest readable sequence and reject expired snapshots explicitly.
- [#1446](https://github.com/nulang-org/nulang/pull/1446) and [#1452](https://github.com/nulang-org/nulang/pull/1452) already implement/test the binary-search snapshot-read optimization and benchmarks on an older draft base. Port only after duplicate-key version ordering/retention semantics are stable, rather than starting another standalone optimization.

### Gate D — bounded storage and SQLite/libSQL-inspired operability

- The earlier LSM line is [#1284](https://github.com/nulang-org/nulang/pull/1284) -> [#1289](https://github.com/nulang-org/nulang/pull/1289) -> [#1298](https://github.com/nulang-org/nulang/pull/1298) -> [#1302](https://github.com/nulang-org/nulang/pull/1302). It contains useful designs, **not** a preapproved merge chain. Rebase selectively after Gates A-C, preserving fallible-read errors, self-contained checkpoint admission, atomic manifests and safe orphan-file cleanup.
- [#1283](https://github.com/nulang-org/nulang/pull/1283) is stacked on an older serving branch; the predecessor [#1278](https://github.com/nulang-org/nulang/pull/1278) was closed as superseded. Preserve its bounded-cache tests and design but do not blindly merge that branch.
- Make WAL index/resident metadata proportional to required history, **not lifetime writes**; introduce bounded SSTable-block caching, version GC, background maintenance and restart-memory limits. Never reclaim data protected by an active snapshot or replication/backup cursor.
- Add SQLite-style **read-only** `quick_check` / `integrity_check` diagnostics. The validator must *not* call the current repairing `FileWal::open` path against evidence files if it could truncate a torn tail. Validate header/frame bounds, checksums, sequence chain, checkpoint metadata, and manifest/SSTable references. Emit typed errors without dumping data or secrets.
- Add a consistent online-backup API only once a durable storage snapshot can pin its manifest generation, WAL sequence and referenced files. Prove restore in a disposable directory with a replay/corruption test; never mistake a file copy for a transactionally consistent online backup.
- Embedded replicas and logical changefeeds come later: one durable authority per tablet, explicit replica cursors, a catch-up/fallback path and measured lag. Neither local process locks nor a successful single-node WAL test proves distributed consistency.

## Benchmark and operational acceptance

For every storage-format or batching change, compare **exact base vs exact candidate** on the same host and filesystem, using pinned toolchain, database size, sync policy and key/value distributions. Record cold/warm results, 1/16/64/256-write batches when supported, transactions/sec, p50/p95/p99 **transaction** latency (not just group latency), RSS/allocated memory, bytes per operation, WAL growth, recovery time, cache hit rate and compaction write amplification. Repeat interleaved base/candidate measurements to separate noise from regressions. Do not assert a speedup merely because a group amortizes one fsync.

A candidate can graduate only if:
1. no acknowledged committed update is lost across the interruption/kill matrix;
2. every corrupt complete frame and invalid durable checkpoint is detected, not ignored;
3. version retention does not change historical snapshot answers or resurrect deleted data;
4. peak and steady-state memory remain bounded in long-duration tests;
5. checkpoint/backup restore demonstrates exact committed state in a disposable store;
6. failures produce actionable typed errors and recoverable operator procedures;
7. the **current exact head** has all required hosted checks and review approvals, with no status bypass.

## Explicit non-goals for this promotion

No multi-Raft, distributed two-phase commit, multi-primary writes, `io_uring`, SQL parser, secondary/vector index, automatic Nulang Cloud persistence switch, production restore on real data, or removal of existing SQLite/libSQL storage backends.

**Owner of next decision:** reviewer selects the source-coherent main promotion topology for #2184/#2199, confirms #2201 exact-head CI, and rejects conflicting overlapping changes before authorizing a merge.
