# Opt-in entry-pressure S3-FIFO eviction

Enable only for disposable memory-mode cache data:

    nulang-fabric --mode memory --eviction s3fifo --max-entries 100000

Default is noeviction, including embedded CacheStore::with_limits.
Embedded callers can explicitly construct CacheStore::with_s3fifo.
The standalone CLI rejects S3-FIFO with either durable mode. No implicit
evictions are recorded in the journal, so this policy must not be used
as a durable key-value database or for correctness-critical cache keys.

This is an entry-count S3-FIFO adaptation: a small probationary FIFO
(target max(1, floor(capacity/10))), a main second-chance FIFO, and a
bounded ghost FIFO containing only 64-bit key fingerprints. A successful
GET (including MGET element reads) increments a saturating 2-bit frequency.
Writes, TTL and existence queries do not count as GETs. On entry pressure,
small entries with frequency greater than one move to main with frequency
zero; other small victims enter ghost history. Main entries with nonzero
frequency are rotated while decrementing the count. A ghost match places
a newly inserted key in main; collisions affect admission placement only,
never key identity. Ghost history is deduplicated and bounded to the
configured entry count. Queue targets are not fixed memory partitions.

Resident links are slot-indexed; deletion, lazy expiry and wheel expiry
unlink the node before slot reuse, and eviction cancels scheduled expiry.
There is no stale resident queue per access. Retained allocation estimates
include eviction node and ghost capacity; this is not a hard RSS limit.
The victim planner examines at most 4096 nodes per admission, including
promotions, rotations and protected targets. It may conservatively reject
a write with available victims beyond that budget; no live data is removed
on planning failure, although policy ordering/frequencies may change.
The current planner uses bounded temporary victim/protection bookkeeping;
hit-path frequency updates require no allocation.

All writes first perform conservative arena admission, ignoring blocks
that might be freed by eviction or later batch replacement. Automatic
eviction addresses entry count only, not arena pressure. A value can be
rejected even if evicting a matching arena size-class block would permit
it. MSET preflights all new keys and allocations, protects existing batch
targets, selects the entire victim set before deleting anything and
applies the writes only after admission. Duplicate keys retain last-write
semantics. Existing durable preflight remains unchanged. The local
metrics endpoint exports cache_evictions_total separately from expiration
and explicit deletion counters.

## Seeded hit-ratio qualification

    cargo run --locked --no-default-features --release --example fabric_s3fifo_trace

Eighteen 100,000-reference traces, 3 seeds, capacities 256 and 1024.
The actual Rust CacheStore is compared with simple exact FIFO and LRU
reference models at equal entry capacities. All keys/values fit inline.
Results include warmup and use a deterministic xorshift trace. Equal
entry capacity does not mean equal total memory usage. No claim about
relative throughput, byte-aware eviction, real production traces, or
hard memory limits follows from this benchmark.

| Capacity | Workload | S3-FIFO | FIFO | LRU |
|---:|---|---:|---:|---:|
| 256 | scan | 59.81% | 39.95% | 49.67% |
| 256 | shift | 78.63% | 65.77% | 78.07% |
| 256 | uniform | 6.14% | 6.19% | 6.19% |
| 1024 | scan | 59.87% | 54.79% | 59.87% |
| 1024 | shift | 79.36% | 76.81% | 79.36% |
| 1024 | uniform | 24.79% | 24.86% | 24.85% |

Results: benchmarks/fabric-s3fifo-trace-20261128.csv. Scan and shifting
hot-set workloads favor S3-FIFO at smaller capacity; uniform references
show no meaningful advantage. Hit and eviction counts matched exactly
across Rust 1.95 and 1.99 runs; retained-capacity estimates differed due
to hash-table capacity behavior. These results justify opt-in testing,
not a default change. Gertrude local qualification used the repository's
pinned Rust 1.95.0 and a focused manifest with lockfile package identities
verified against the repository lock. Full workspace and independent
reference-server qualification remain outstanding.
