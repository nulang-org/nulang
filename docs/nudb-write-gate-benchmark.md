# NuDB write-gate latency benchmark

**Status:** Opt-in instrument, not a validated performance result or CI threshold.

The single-node WAL ownership protocol adds one advisory filesystem gate
(`.nudb-write-gate.lock`) held across WAL writes, fsync, and, at the
`WalBackedTablet` layer, MVCC publication. The benchmark estimates how much
**directory-wide serialization** affects independent tablet WALs.

## Run

Requires the project's pinned Rust toolchain (see `rust-toolchain.toml`) on a
local filesystem that supports `File::lock`, and enough free disk space for
the WAL workload.

```sh
cargo test --no-default-features --example nudb_gate_bench
cargo run --release --no-default-features --example nudb_gate_bench -- \
  --iterations 500 --threads 4 --warmup 20
```

The example emits JSON records for two conditions:

| Condition | Workload |
|---|---|
| `isolated_directory_gates` | One WAL and one gate per worker directory |
| `shared_directory_gate` | One WAL per worker, but all writers contend on the same directory gate |

Both use `FileWal::append_write` with identical constant-size payloads and
durable `sync_data`. The benchmark excludes setup and initial warmup from
measured append time. Each condition reports successful write count, mean,
nearest-rank P50/P95/P99 and maximum append latency in microseconds, elapsed
wall time and aggregate operations per second.

To test meaningful contention, run multiple repeated trials (e.g. 1, 2, 4,
8 threads) on the same filesystem. Alternate execution order between trials;
the included tool currently measures the isolated case first to keep output
consistent. Record OS, filesystem, disk medium, power-loss/fsync mode,
CPU count, Rust toolchain, build profile and total write count. Compare
paired trial distributions, not one lucky run. Disable other noisy workloads
during reference measurements.

## Interpretation and limitations

- This measures **end-to-end WAL append + fsync** under different contention
  topologies, not the isolated cost of an advisory lock syscall.
- Baseline writes are **not lock-free**: each WAL already takes a gate. The
  comparison isolates the cost of sharing a directory-wide gate across
  otherwise independent WALs.
- Disk fsync serialization, scheduler effects, page-cache pressure and
  filesystem metadata behavior all affect results. Microsecond ratios are
  not portable across disks or machines.
- Fewer WALs per directory, per-tablet gates, group commit and transaction
  batching may reduce contention, but any optimization must preserve the
  owner-acquisition ordering established by PR #1430.
- This benchmark does **not** validate physical crash safety, distributed
  consensus or cross-node ownership epochs.

## Promotion criteria

Do not use benchmark performance to justify changing the locking protocol
until exact-head correctness tests (snapshot scan, split recovery, fail-stop
owner tests, WAL checkpoint/reclaim, managed-WAL guard and write-gate tests)
pass and one thread-contended, repeated benchmark is recorded.

The example's percentile and CLI-boundary tests are deterministic and may be
run in CI; the timing run is deliberately manual and has no flaky threshold.
