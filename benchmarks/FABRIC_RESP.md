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
fewer than 24 cases is incomplete. The test contract can be checked with:

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
