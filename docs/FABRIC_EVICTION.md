# Opt-in S3-FIFO eviction

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
Writes, TTL and existence queries do not count as GETs. On entry or arena pressure,
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

Normal conservative admission is attempted first. When it fails and
S3-FIFO is enabled, the planner calculates the allocation demand by
arena size class (every value and every distinct new key), the free
blocks in each class, and remaining initialized arena extent. It selects
victims until both entry pressure and required additional extent fit.
Once entry pressure is satisfied, entries that cannot reduce outstanding
class demand are skipped. Protected batch targets and the key being
overwritten cannot be selected. No live key is deleted until the complete
plan succeeds. The 4096-node budget still includes skipped nodes,
promotions and rotations. A rejected plan preserves values, TTLs and
eviction counts, although policy order and frequencies can change.
Victim bookkeeping is bounded by the scan budget; key/protection
bookkeeping is proportional to the input batch.

Eviction frees reusable blocks, not initialized arena extent. Different
classes cannot be coalesced, and existing protected values or values
replaced earlier in a batch are not counted as available blocks.
Fragmentation, conservative replacement accounting and the scan limit
can therefore still cause OOM. There is no compaction or hard RSS
guarantee. Duplicate MSET keys retain last-write semantics and admission
is atomic with respect to all visible values. Existing noeviction and
durable preflight behavior remains unchanged. The local metrics endpoint
exports cache_evictions_total separately from expiration and explicit
deletion counters.

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


## Arena-pressure qualification

    cargo run --locked --no-default-features --release --example fabric_arena_eviction_trace

Compare the actual entry-only store from b62aea6d8 with the arena-aware
candidate using the same trace source, separate isolated cache-only
build targets, pinned Rust 1.95 and rustc-hash 2.1.3. Twelve traces per
revision, each 100,000 references, three seeds, 1024 entry cap, 8/32 KiB
arena caps, constant 64-byte values or mixed 32–256-byte values and
occasional 32-byte keys. Each admitted key is read back and compared
when hit. These are synthetic reference streams, not throughput tests.

| Arena | Values | Entry-only hit rate | Arena-aware hit rate | Entry-only rejected misses | Arena-aware rejected misses |
|---:|---|---:|---:|---:|---:|
| 8 KiB | 64 B | 31.84% | 18.47% | 204,099 | 0 |
| 8 KiB | mixed | 18.20% | 4.25% | 245,195 | 0 |
| 32 KiB | 64 B | 59.28% | 59.86% | 120,630 | 0 |
| 32 KiB | mixed | 51.41% | 59.83% | 144,961 | 0 |


Raw results: benchmarks/fabric-arena-entry-only-20261128.csv and
benchmarks/fabric-arena-aware-20261128.csv. Across the 1.2 million
references per revision, the entry-only policy rejected 714,885 misses,
whereas the candidate admitted every miss in these specific traces.
All 18 previous inline traces retain the same hit/eviction counts.

The tight 8 KiB arena cases demonstrate a real tradeoff: a static
probation target of 102 entries (10% of the configured 1024 entry cap)
is disproportionate to effective resident capacity, especially with
mixed-size values. The baseline freezes after filling the arena and
can retain some hot keys by rejecting all later misses; the candidate
keeps admitting and may churn hot keys. The 32 KiB traces improve
hit rate. These results do not establish universal hit-rate improvement
or equal total memory consumption. Dynamic/byte-aware queue sizing,
compaction or alternative arena layout, and larger real-world traces
remain separate design questions. No default change is warranted.
