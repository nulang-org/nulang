# Nulang Performance Crown

This document defines the evidence Nulang needs before making broad performance claims. The canonical machine-readable coverage contract is `benchmarks/performance_crown.json`; `scripts/performance_crown.py` validates and summarizes it.

The suite is intentionally not a single leaderboard. Distributed-language performance has multiple bottlenecks, so evidence is grouped into five domains:

1. **Local** — hot-loop execution, code generation, allocation, collections, and call overhead.
2. **Concurrent** — actor spawn, send/ask, mailbox admission, scheduling, ping-pong, fan-out/fan-in, and shard scaling.
3. **Distributed** — serialization, remote send/ask, node-to-node throughput, convergence, and failure recovery.
4. **Durable** — journal/checkpoint cost, atomic durable transitions, recovery, timers, signals, and saga execution.
5. **Application** — end-to-end HTTP, distributed KV, and durable workflow services.

## Status meanings

- `microbenchmark`: a Nulang benchmark exists, but it isolates only part of an operation and is not sufficient for a cross-runtime performance claim.
- `end_to_end`: the complete Nulang operation is measured across its relevant runtime boundaries, but matched external baselines are not yet implemented.
- `comparative`: a controlled cross-runtime harness exists with explicit baselines.
- `planned`: this evidence is still missing.

A comparative workload is marked `claim_ready` by the reporting script only when it has at least one non-Nulang baseline and uses `measurement_mode=controlled`. This means the harness is suitable for collecting comparative evidence; it does **not** mean Nulang wins that workload. `end_to_end` measurements remain explicitly non-comparative even when the manifest records intended future baselines.

## Commands

```bash
python3 scripts/performance_crown.py --check
python3 scripts/performance_crown.py
python3 scripts/performance_crown.py --json
python3 -m unittest scripts/tests/test_performance_crown.py
cargo test --locked --test distributed_remote_roundtrip
cargo bench --bench bench_main -- 'dist/remote_actor_roundtrip'
```

The repository-wide Python harness test job already discovers `scripts/tests/test_*.py`, so changes that invalidate the manifest fail CI.

## Current evidence boundary

The controlled cross-runtime Savina messaging suite remains the only `comparative` entry. Local JIT/AOT, actor hot-path, NUL0 codec, and persistence measurements are useful microbenchmarks but must not be generalized into whole-language or end-to-end distributed-system claims.

`distributed.remote_message_roundtrip` is now an `end_to_end` Nulang measurement. Its steady-state loopback fixture uses two real TCP-backed `Runtime`s and one actor on each node. One measured iteration sends a one-value remote actor message left→right, processes NUL0 decode/routing/mailbox/scheduler/native-handler work, then performs the same return hop right→left. Cluster bootstrap, TCP connection establishment, and the first NUL0 handshake are warmed before timing. This closes the Nulang-side measurement gap but does **not** justify a Go/Erlang comparison until matched baselines are run under one controlled harness.

The highest-priority remaining evidence is:

1. matched Go/Erlang baselines for `distributed.remote_message_roundtrip`
2. `durable.atomic_transition`
3. `application.http_service`
4. `application.distributed_kv`
5. `application.durable_workflow_service`

These gaps should be closed before adding broad performance marketing claims. New optimizations should attach to one or more manifest workloads so their intended effect and measurement boundary are explicit.
