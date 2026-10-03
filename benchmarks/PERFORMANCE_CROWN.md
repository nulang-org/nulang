# Nulang Performance Crown

This document defines the evidence Nulang needs before making broad performance claims. The canonical machine-readable coverage contract is `benchmarks/performance_crown.json`; `scripts/performance_crown.py` validates and summarizes it.

The suite is intentionally not a single leaderboard. Distributed-language performance has multiple bottlenecks, so evidence is grouped into five domains:

1. **Local** — hot-loop execution, code generation, allocation, collections, and call overhead.
2. **Concurrent** — actor spawn, send/ask, mailbox admission, scheduling, ping-pong, fan-out/fan-in, and shard scaling.
3. **Distributed** — serialization, remote send/ask, node-to-node throughput, convergence, and failure recovery.
4. **Durable** — journal/checkpoint cost, atomic durable transitions, recovery, timers, signals, and saga execution.
5. **Application** — end-to-end HTTP, distributed KV, and durable workflow services.

## Status meanings

- `microbenchmark`: a Nulang benchmark exists, but it is not sufficient for a cross-runtime performance claim.
- `comparative`: a controlled cross-runtime harness exists with explicit baselines.
- `planned`: this evidence is still missing.

A comparative workload is marked `claim_ready` by the reporting script only when it has at least one non-Nulang baseline and uses `measurement_mode=controlled`. This means the harness is suitable for collecting comparative evidence; it does **not** mean Nulang wins that workload.

## Commands

```bash
python3 scripts/performance_crown.py --check
python3 scripts/performance_crown.py
python3 scripts/performance_crown.py --json
python3 -m unittest scripts/tests/test_performance_crown.py
```

The repository-wide Python harness test job already discovers `scripts/tests/test_*.py`, so changes that invalidate the manifest fail CI.

## Current evidence boundary

Today, the controlled cross-runtime Savina messaging suite is the only `comparative` entry. Local JIT/AOT, actor hot-path, NUL0 codec, and persistence measurements are useful microbenchmarks but must not be generalized into whole-language or end-to-end distributed-system claims.

The highest-priority missing evidence is:

1. `distributed.remote_message_roundtrip`
2. `durable.atomic_transition`
3. `application.http_service`
4. `application.distributed_kv`
5. `application.durable_workflow_service`

These gaps should be closed before adding broad performance marketing claims. New optimizations should attach to one or more manifest workloads so their intended effect and measurement boundary are explicit.
