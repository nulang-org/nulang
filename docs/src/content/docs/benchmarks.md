---
title: Performance benchmarks
description: Measured Nulang actor throughput against Rust channels, Go goroutines, and Erlang/BEAM, with reproducible methodology and unmeasured actor-framework comparisons.
---

This page reports **observations**, not projected speedups or claims that one language is universally faster. The latest retrieved cross-runtime run was completed **12 October 2026 (UTC)** on a GitHub-hosted Ubuntu runner. Results are a **point-in-time snapshot**, not a continuously updated leaderboard.

## Measured language and runtime comparison

These are **median logical messages per second** across three measured repetitions (one warm-up). Higher is better **within each workload**. All four runtimes were limited to the same single logical CPU; each used the [repository's matched Savina-style fixtures](https://github.com/nulang-org/nulang/tree/main/benchmarks/cross_runtime).

| Workload | Nulang actors | Rust std channels | Go channels | Erlang/BEAM processes |
|---|---:|---:|---:|---:|
| Counting (200,000 messages) | 1,148,869 | 48,298,352 | 29,710,341 | 10,640,421 |
| Ping-pong (20,000 round trips) | 594,970 | 251,192 | 6,106,795 | 3,371,477 |
| Thread-ring (10 actors, 20,000 hops) | 667,255 | 221,973 | 6,223,593 | 3,323,684 |
| Fork-join (8 workers, 50,000 tasks) | 713,909 | 42,148,601 | 31,158,385 | 9,393,979 |

**What these numbers say:** In this run, Nulang is behind Go and Erlang/BEAM on every listed workload and behind Rust on counting and fork-join. It is ahead of this particular Rust **native-thread + standard-channel** implementation on ping-pong and thread-ring. That is **not** evidence Nulang is faster than optimized Rust actor runtimes: native threads incur different scheduling costs from lightweight actors.

### Measurement details and reproducibility

| Property | Recorded configuration |
|---|---|
| Source | [Successful workflow run #38193394276](https://github.com/nulang-org/nulang/actions/runs/38193394276) · [result artifact](https://github.com/nulang-org/nulang/actions/runs/38193394276/artifacts/11722402594) |
| Run date | 2026-10-12 UTC |
| Source revision | Workflow artifact generated for checkout SHA `ca509b037ff4d7b5c3351a3590ede6b08e41f87d` (pull-request test checkout) |
| Host | GitHub-hosted Ubuntu 24.04 x86-64 runner (shared hardware) |
| CPU configuration | `--cpu-mode single`, affinity `[0]`, Nulang one runtime shard |
| Runs | 1 warm-up, 3 measured repetitions; median reported |
| Nulang | Standalone `nulang-savina` optimized profile, bytecode actors |
| Rust | rustc 1.99.0, `std::sync::mpsc` plus native OS threads |
| Go | Go 1.25.14, channels and goroutines |
| Erlang | Erlang/OTP 25.3.2, native processes/mailboxes |

To repeat the suite on a Linux host with Rust, Go, Erlang and Python installed:

```bash
python3 scripts/cross_runtime_bench.py \
  --runs 5 --warmup 1 --cpu-mode single \
  --output cross-runtime-results.json
```

The output JSON includes runtime and toolchain metadata, input counts, individual timings and medians. The recorded run above used **three** measured repetitions; the command uses five for a more useful new measurement. See the [benchmark contract and limitations](https://github.com/nulang-org/nulang/blob/main/benchmarks/cross_runtime/README.md) and the [runner implementation](https://github.com/nulang-org/nulang/blob/main/scripts/cross_runtime_bench.py).

:::caution[Interpretation]
These are **Savina-style** fixtures, not canonical Savina scores. The Rust baseline deliberately uses native threads and standard channels rather than a Rust actor framework. Nulang's counting/fork-join harness queues a burst before draining its scheduler, whereas other runtimes can consume while producing. Shared runners vary under load; this snapshot is **not** a controlled multicore, production service, durability, or network benchmark. CPU affinity matches the available CPU budget, not all scheduling semantics.
:::

## Actor-framework comparison status

**Do not infer framework performance from the language results above.** Actors, mailboxes, runtimes, supervision and scheduler policies carry costs not captured by generic language microbenchmarks.

| Language / actor framework | Implementation being compared | Matched numerical result |
|---|---|---|
| **Nulang** | Nulang bytecode actors and runtime scheduler | **Measured** in table above |
| **Erlang/OTP** | BEAM processes and mailboxes (without measuring OTP supervisor lifecycle) | **Measured primitives**; full OTP workloads not measured |
| **Rust standard library** | Native threads with `std::sync::mpsc` | **Measured language baseline**, not an actor framework |
| **Go standard library** | Goroutines with channels | **Measured language baseline**, not an actor framework |
| [Apache Pekko](https://pekko.apache.org/) / [Akka](https://akka.io/) | JVM actor frameworks | Not yet measured on the matched suite |
| [Actix](https://github.com/actix/actix) | Rust actor framework | Not yet measured on the matched suite |
| [Pony](https://www.ponylang.io/) | Actor-native programming language | Not yet measured on the matched suite |
| [Proto.Actor](https://proto.actor/) | Actor framework for Go and .NET | Not yet measured on the matched suite |

The next fair actor-framework comparison should pin each dependency/runtime version and use identical message counts and payloads, a single enforced CPU followed by a **separate** multicore scaling suite, measured startup/allocation alongside steady-state throughput, and several repetitions on dedicated hardware. Supervision/restart, actor density, cross-node messaging and persistent state require their **own** workload definitions rather than borrowing the results here.

## Additional Nulang benchmark coverage

- [Criterion performance history](https://github.com/nulang-org/nulang/tree/automation/benchmark-history/benchmarks) — per-commit VM, JIT, scheduler, GC and other microbenchmarks; useful for regression detection, **not** for cross-host rankings.
- [Nulang Savina-style suite](https://github.com/nulang-org/nulang/blob/main/benchmarks/SAVINA.md) — counting, ping-pong, thread-ring, fork-join and a smaller-than-canonical Skynet workload.
- [Shard scaling methodology](https://github.com/nulang-org/nulang/blob/main/benchmarks/README.md#nulang-shard-scaling) — dedicated same-shard, multi-worker diagnostics. Not directly comparable to the single-CPU cross-runtime table.
- [Execution architecture](../language/performance/) — bytecode reference, Cranelift JIT, restricted AOT and experimental WASM backend.

**Bottom line:** Nulang's runtime is measurable and the baselines are reproducible, but it is not yet competitive with the tested Go/BEAM primitives on these four single-CPU workloads. No claim of superiority over Akka, Pekko, Actix, Pony or Proto.Actor is supported by current matched measurements.
