# Fabric RESP comparative baseline

This is a small, reproducible loopback baseline, not a production capacity
claim or a regression threshold. It compares identical validated RESP
requests against optimized Fabric and Valkey. Build the actual candidate
with the locked release profile where resources permit:

    cargo build --locked --release --no-default-features --features cache-server --bin nulang-fabric
    python3 scripts/fabric_resp_ab_bench.py \
      --fabric target/release/nulang-fabric \
      --valkey /usr/bin/valkey-server \
      --journal-dir /path/to/local/disk \
      --output /tmp/fabric-resp-ab.json

The journal directory must exist and have space for temporary journals.
The script uses private temporary directories and loopback listeners only,
starts fresh processes for every sample, verifies every response, warms up
each workload, alternates engine order, and records raw per-round summaries
and binary SHA-256 hashes. Failed responses abort the run. An output with
fewer than 24 cases is incomplete for the default matrix; selected --mode/--workload runs declare expected_cases. The test contract can be checked with:

    python3 -m unittest discover -s scripts/tests -p test_fabric_resp_ab_bench.py

## Scope and semantics

- One client, one key, 64-byte value; GET, SET, TTL GET, TTL SET.
- Pipeline depth 1 and 32; three independent rounds, 4096 measured commands
  per memory sample and 256 per durable sample.
- Report latency per entire request/reply batch, not per-command service
  latency. At depth 32 the durable sample contains only eight batches:
  its p99/p99.9 are descriptive maxima, not reliable tail estimates.
- Memory: persistence disabled for both engines. Buffered: Fabric buffered
  WAL versus Valkey appendonly/everysec. Sync: Fabric data-synced WAL per
  mutation versus Valkey appendonly/always, which can amortize work across
  an event-loop batch. These durability policies are not identical.
- Fabric checkpointing and Valkey automatic AOF rewrite are disabled; no
  TLS, concurrent clients, keyspace scaling, eviction or metrics scraping.
- Fabric additionally persists observed TTL clock floors; Valkey is not
  configured to provide the same cross-restart rollback protection.
  Do not remove that correctness property solely to win a benchmark.
- The initial run used a release-optimized focused harness importing the
  exact candidate runtime modules, with dependencies resolved to the
  audited repository lock versions. It is not a full-workspace release
  artifact. The local Btrfs disk was nearly full and the host was not
  exclusively reserved. Disk latency is particularly environment-dependent.

## Optimization priorities

1. Preserve crash/restart TTL semantics while reducing repeated clock
   publication. Investigate a bounded lease or watermark strategy only
   after specifying the allowed clock regression and crash window.
2. Batch durable mutations with explicit acknowledgement and recovery
   semantics, qualifying partial-write and synchronization failures.
3. Repeat with a full release binary, dedicated hardware, multiple key
   distributions and client counts, long enough for tail estimates,
   CPU/RSS sampling, and separate TLS and checkpoint workloads.

The raw initial baseline is recorded in fabric-resp-baseline-20261127.json.

### Initial run: median operations per second

| Mode | Workload | Pipeline | Fabric | Valkey |
|---|---|---:|---:|---:|
| memory | get | 1 | 62,915 | 58,565 |
| memory | get | 32 | 1,178,715 | 959,020 |
| memory | set | 1 | 60,549 | 52,228 |
| memory | set | 32 | 1,135,631 | 896,184 |
| memory | ttl-get | 1 | 62,943 | 55,818 |
| memory | ttl-get | 32 | 1,170,788 | 916,540 |
| memory | ttl-set | 1 | 57,637 | 58,690 |
| memory | ttl-set | 32 | 1,001,747 | 640,991 |
| buffered | get | 1 | 53,092 | 49,536 |
| buffered | get | 32 | 1,074,348 | 788,041 |
| buffered | set | 1 | 39,220 | 39,071 |
| buffered | set | 32 | 185,210 | 525,603 |
| buffered | ttl-get | 1 | 688 | 48,097 |
| buffered | ttl-get | 32 | 509 | 899,726 |
| buffered | ttl-set | 1 | 166 | 36,976 |
| buffered | ttl-set | 32 | 297 | 416,409 |
| sync | get | 1 | 55,367 | 49,786 |
| sync | get | 32 | 1,118,950 | 908,665 |
| sync | set | 1 | 178 | 519 |
| sync | set | 32 | 843 | 15,381 |
| sync | ttl-get | 1 | 102 | 49,312 |
| sync | ttl-get | 32 | 282 | 890,834 |
| sync | ttl-set | 1 | 60 | 111 |
| sync | ttl-set | 32 | 212 | 20,329 |

The measured memory path is competitive on this narrow workload; the durable TTL floor and per-command sync path dominate the slow cases. These are candidates for profiling, not established causal attribution. The benchmark is intentionally not a CI performance gate.


## Clock reservation follow-up

The optional clock-reservation policy persists an upper watermark before
serving covered observations. With --fabric-clock-reservation-ms 1000,
Fabric can reuse that watermark for up to one second of observed time
while preserving the fail-closed restart rule. Zero (the default) retains
exact-floor publication. Restart may be refused until the wall clock
reaches the watermark, even after orderly shutdown. No durability claim
is relaxed to obtain these results.

The follow-up used the same host, workload, three rounds, and optimized
focused harness methodology, with --mode buffered --mode sync
--workload ttl-get --workload ttl-set --fabric-clock-reservation-ms 1000.
It is a separate run, not a paired exact-versus-reserved trial. Disk load
and Valkey throughput varied between runs. Small samples do not support
tail-latency claims. Raw results: fabric-clock-reservation-20261127.json.

| Mode | Workload | Pipeline | Prior exact Fabric ops/s | Reserved Fabric ops/s | Follow-up Valkey ops/s |
|---|---|---:|---:|---:|---:|
| buffered | ttl-get | 1 | 688 | 51,503 | 50,030 |
| buffered | ttl-get | 32 | 509 | 1,133,701 | 877,716 |
| buffered | ttl-set | 1 | 166 | 38,110 | 38,436 |
| buffered | ttl-set | 32 | 297 | 177,936 | 412,037 |
| sync | ttl-get | 1 | 102 | 54,700 | 48,674 |
| sync | ttl-get | 32 | 282 | 1,144,932 | 884,604 |
| sync | ttl-set | 1 | 60 | 248 | 236 |
| sync | ttl-set | 32 | 212 | 263 | 7,886 |

TTL GET and buffered TTL SET improve substantially; synchronous SET still pays per-mutation WAL synchronization. Group commit is the next bottleneck to address. No production capacity claim.


## Bounded synchronous group commit follow-up

With --fabric-group-commit and --fabric-clock-reservation-ms 1000, the
synchronous reactor coalesces at most one 32-command connection turn
into one WAL data synchronization. The entire turn is withheld from
the socket until the WAL boundary succeeds. This is not a transaction
and does not change recovery of individual complete records. No artificial
waiting interval is introduced. The raw follow-up is in
fabric-group-commit-20261127.json.

| Workload | Pipeline | Prior Fabric ops/s | Grouped Fabric ops/s | Follow-up Valkey ops/s |
|---|---:|---:|---:|---:|
| set | 1 | 178 | 1,037 | 1,060 |
| set | 32 | 843 | 27,184 | 31,298 |
| ttl-set | 1 | 248 | 1,307 | 1,301 |
| ttl-set | 32 | 263 | 35,052 | 38,845 |

Separate runs on the same Fedora host, not a controlled paired before/after trial. Three repeats per case, optimized focused harness, real Btrfs disk, small hot-key workload. For non-TTL SET the prior baseline used exact clock mode; plain SET does not need clock publication. No full-workspace release binary, concurrency, power-loss or reliable p99 claim. Grouped mode is opt-in and only applies to synchronous WAL records within one connection turn; no cross-client batching.
