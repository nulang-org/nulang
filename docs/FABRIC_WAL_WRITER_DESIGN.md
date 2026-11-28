# Fabric WAL writer decision record

Status: investigation, not an implementation commitment.

## Question

Can a dedicated writer improve synchronous durability throughput and tail
latency without making acknowledgement safety, visibility, expiry, memory
admission, and checkpoint recovery more complex than the benefit warrants?

The previous instrumented 16-client run attributed 12.21 seconds of
measured duration to WAL sync attempts in a 30.12-second workload. That
motivates a controlled comparison before changing ownership.

## Measurement

Run the same optimized instrumented executable with identical bounded
workloads and synchronous durability policy, on a disk-backed directory
and a volatile tmpfs directory. Compare per-connection and cross-client
group commit. Reverse the case order on alternate rounds. Each sample
validates every reply, checks memory/admission limits, exercises automatic
checkpoint and TTL churn, kills its own process, and verifies the
acknowledged state on reopening the same journal.

    python3 scripts/fabric_wal_storage_ab.py \
      --fabric target/release/nulang-fabric \
      --disk-dir /path/to/local-disk \
      --ram-dir /dev/shm \
      --output /tmp/fabric-wal-ab-new.json \
      --rounds 2 --seconds 12 --clients 16 --batches-per-second 20

The tmpfs case is only a negative control for storage latency. It does
not provide persistent durability under guest/host loss. This is not an
isolated disk benchmark: filesystems differ, the device may be shared,
and closed-loop offered load is bounded. Short samples are exploratory,
not a release performance gate. Do not compare success under SIGKILL on
tmpfs to physical power loss. The harness requires fresh private
directories, distinct filesystems, sufficient free space, and an absent
output path.

## Why not simply move sync_data to a worker?

Existing durable commands apply mutations to the shard-local store before
a group sync. Currently all affected replies (including dependent reads)
are withheld, no other group executes before synchronization, and any
uncertainty poisons the shard. Merely allowing reads or writes while an
asynchronous sync is pending breaks the current visibility boundary or
requires a more complex pending-commit model.

A worker that offloads only sync_data and then immediately blocks the
reactor on a completion channel adds scheduling/ownership overhead
without removing the synchronous critical path.

Two different objectives require different approaches:

1. Reduce the number of syncs per mutation. Existing ready-client
   batching already amortizes a sync; a bounded wait for more clients
   can trade latency for batch fullness. Measure the per-sync record
   distribution and saturation before adjusting the existing caps.
2. Keep unrelated work responsive during slow I/O. A bounded single
   in-flight commit barrier can allow transport admission/reads while
   withholding all application dispatch/replies until success. This
   can improve transport responsiveness without speculative execution,
   but does not increase same-shard mutation throughput by itself.

## Minimal candidate if further measurement warrants it

Use a single owning reactor and one worker, an explicit generation-tagged
in-flight barrier, one bounded request slot, and one completion slot.
The worker may synchronize a safely owned WAL handle only; no concurrent
append, checkpoint, clock publication, store mutation, or file replacement
during that in-flight barrier. Retain the exclusive writer guard until
completion. Never expose the duplicated handle outside the owner protocol.
No speculative execution or durable metadata format changes.

While waiting, admit bounded transport data using existing budgets, but
do not dispatch any application command or flush dependent responses.
Do not spin on writable readiness for withheld output. No expiry sweep
or automatic checkpoint that changes durable state during the barrier.
On a matching success, release withheld responses in connection order,
preserve deferred readiness, and resume the normal reactor. On failure,
missing completion, unexpected generation or worker panic, fail closed.
A timeout cannot cancel a kernel fsync; do not permit another writer or
checkpoint to start while the prior worker may still own the journal.
Shutdown must wait for completion or terminate the process with the
normal uncertainty semantics. No unbounded queue, retry of ambiguous
operations or durability downgrade.

A later true overlapped writer/commit pipeline requires a separate
committed-state frontier or staged mutations with explicit ordering,
admission, expiry, read visibility and recovery rules. Do not build
this until a bounded barrier and longer controlled experiments establish
a real workload benefit.

## Acceptance gates

- Exact acknowledgement ordering across clients and dependent reads.
- Injected sync failure, worker panic/disconnect, shutdown, checkpoint
  exclusion, memory limits, partial response and TLS continuation.
- Read-only groups do not schedule a sync.
- Exact-count and sequence verification after abrupt process/VM loss.
- Compare identical binaries/modes and controlled storage, with sustained
  workload throughput and tail latency; quantify worker overhead and
  reject if it only adds complexity.
- No new runtime dependencies, relaxed durability mode, default
  behavior change, or release until hosted checks and review pass.

## Initial results

Two rounds, 12 seconds each, 16 clients, 20 batches/s per client, 16 commands/batch. All eight cases passed including acknowledged-state recovery and idle reclamation. Median throughput and record counts across two samples are descriptive, not statistical estimates of sustained capacity. P99 ranges below are ranges of histogram bucket upper bounds across samples.

| Storage / commit mode | Median ops/s | Records/sync | Batch p99 upper bound (ms) | WAL sync p99 upper bound (ms) |
|---|---:|---:|---:|---:|
| disk/connection | 3,311 | 12.0 | 500–1000 | 25–50 |
| disk/cross | 5,087 | 53.6 | 10–50 | 2.5–25 |
| ram/connection | 5,096 | 12.0 | 1–2 | 0.005–0.005 |
| ram/cross | 5,097 | 14.2 | 2–2 | 0.005–0.005 |

The disk-backed cross-client cases approached the workload ceiling; per-connection cases did not. The tmpfs negative control also approached the ceiling and exhibited much lower sync latencies. Prioritize configurable bounded coalescing experiments and dedicated-disk testing before changing the reactor/WAL ownership protocol. Existing caps and defaults are unchanged. A writer thread cannot itself eliminate storage synchronization latency. Raw data: benchmarks/fabric-wal-storage-ab-20261127.json. Host was shared Fedora/Btrfs with a nearly full disk; samples are short and do not prove causality, maximum throughput, or physical durability.
