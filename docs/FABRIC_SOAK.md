# Fabric sustained workload qualification

The opt-in Linux soak harness exercises the standalone synchronous
cache, cross-client group commit, reserved TTL clock, automatic
checkpointing, and operational metrics. It runs only its own loopback
server processes in a fresh private temporary directory and verifies
every response. It requires Python 3.11+ and a trusted local executable.

Run, for example:

    python3 scripts/fabric_soak.py \
      --fabric target/release/nulang-fabric \
      --work-dir /path/to/local/test-directory \
      --output /tmp/fabric-soak-new-report.json \
      --clients 16 --seconds 90 --batches-per-second 20

The report path must not exist. Use a disk-backed work directory with
at least 128 MiB free. A release binary is preferable; record the build
profile and exact binary hash when interpreting the report. Duration
is configurable from 10 to 3600 seconds, client count from 1 to 16,
and offered load from 1 to 100 batches per second per client.

Each worker owns a persistent counter, a persistent 256-byte value,
and a ring of 128 expiring keys (250 ms TTL). Each 16-command batch
contains four iterations of INCR, SET, GET, and TTL SET. The worker
validates the exact replies and records the acknowledged counter
frontier. No concurrent client writes another client's keys.
The workload is closed-loop and rate limited; after a slow batch
it does not attempt to catch up by bursting.

The standalone instance uses a 2 MiB journal, 60 percent checkpoint
threshold, 100 ms checkpoint retry interval, 16 MiB arena limit,
4096-entry physical cache limit, and an 8 MiB shared connection
buffer budget. The harness scrapes integer metrics approximately
once per second, checks health, readiness and freshness, the configured
journal and connection budget limits, physical entries and arena
extent, actual journal file length, and unexpected resource failures.
It samples Linux process RSS and retained cache capacity, not hard
RSS limits. It requires at least one checkpoint and expiration, so a
very low offered load or short duration may intentionally fail coverage.

After workers drain, the harness allows the cache to expire its TTL
keys and publish a final metrics snapshot. Exactly two persistent
entries per client must remain, with no live connections or retained
connection-buffer charge. The harness kills only its own server
with SIGKILL, waits for the optional future clock watermark, and
reopens the same private journal. It verifies every acknowledged
counter, every latest persistent value, absence of all TTL slots,
and successful subsequent mutation. The result is marked passed only
after all these checks. No production data or host limits are changed.

Latency is measured as closed-loop batch round-trip time with a
fixed-size histogram. Reported quantiles are bucket upper bounds
(or null if the quantile falls in the overflow bucket); maximum is
recorded separately. There is no unbounded per-command latency list.
This is not an open-loop load generator or a single-operation latency
measurement. Rate limits, Python overhead, shared CPU/disk activity
and coordinated omission limit interpretation of throughput and tail
latency. Sampling does not establish continuous resource maxima.
A 90-second test is initial sustained-workload evidence, not a long
production soak or proof of a stable RSS ceiling. Multi-hour runs,
larger working sets, client reconnect churn and mixed TLS workloads
remain separate qualification steps.

The standard-library contract tests run without any live server:

    python3 -m unittest discover -s scripts/tests -p test_fabric_soak.py

## Initial measured run

The 90-second, 16-client run completed 440,256 validated
commands at 4,873 ops/s (rate-limited). It completed
68 checkpoints and observed
110,064 expirations. Peak sampled
RSS was 3,846,144 bytes; maximum sampled physical
entries 508; maximum sampled journal
file length 1,260,431 bytes. Final
physical entries were 32, with zero
connection-buffer charge. All acknowledged state and TTL absence
checks passed after SIGKILL recovery. Batch p99 was in the bucket
with upper bound 200000 microseconds.

Raw report: benchmarks/fabric-soak-20261127.json. This was a shared
Fedora/Btrfs host, using the same optimized focused executable as
the VM qualification, not a full-workspace release build. This
run does not certify production throughput, continuous peak
memory, or multi-hour stability.


## Internal latency profile

Add --latency-metrics to the soak command when using a binary with
the optional runtime histograms. The harness validates the cumulative
bucket contract on each scrape, keeps regular resource samples, and
records the final internal command, WAL sync, clock publication and
checkpoint distributions. This does not turn the closed-loop batch
latencies into individual operation latencies or establish causal
correlation. See RESP_CACHE_ARCHITECTURE.md for exact timing boundaries.

### Initial instrumented profile

16 clients for 30 seconds, 149,648 validated commands, 4,968 ops/s, all recovery checks passed. Internal counts match dispatch, group sync and checkpoint counters. Raw report: benchmarks/fabric-latency-profile-20261127.json.

| Operation | Count | Median upper us | p95 upper us | p99 upper us | Max us |
|---|---:|---:|---:|---:|---:|
| command | 149649 | 10 | 25 | 25 | 39995 |
| wal_sync | 1979 | 2500 | 25000 | 25000 | 39973 |
| clock_publish | 31 | 2500 | 50000 | 50000 | 39963 |
| checkpoint | 23 | 5000 | 50000 | 50000 | 42607 |

A shared host and nearly full disk were used. Distributions are not paired per request; do not infer exact causal attribution or overhead from the previous run. The observed profile motivates controlled storage-latency experiments before architectural changes.

For a same-binary storage and group-commit comparison, see [WAL writer decision record](FABRIC_WAL_WRITER_DESIGN.md). The soak also accepts --commit-mode connection or cross (default cross).
