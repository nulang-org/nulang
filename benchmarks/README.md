# Benchmark Results

Machine-readable Criterion output from CI's `bench` job, one JSON file per
`main`-branch commit that ran it. Human-readable HTML reports (Criterion's
own `target/criterion/**/report/index.html`) are not checked in — they're
large and regenerate trivially from the raw estimates here.

## What's tracked

Each `<short-sha>.json` is the concatenated `estimates.json` output Criterion
writes per benchmark under `target/criterion/<bench>/<function>/`, one line
per benchmark, produced by `scripts/collect_bench_results.py` (see that
script for the exact schema: `{benchmark, mean_ns, mean_ns_lower,
mean_ns_upper}` per line).

## Status: regression-gated against a rolling window

The CI `bench` job (`.github/workflows/ci.yml`) runs `cargo bench` on every
push to `main`, collects results here and as a build artifact, then runs
`scripts/check_bench_regression.py` against them. The job fails (after
still committing the new result, so history stays complete) if any
benchmark regresses beyond a threshold set from *that benchmark's own*
historical spread — not a flat percentage. GitHub Actions' shared runners
have enough run-to-run noise (commonly 20-50%+ on wall-clock-sensitive
benchmarks, from neighbor CPU/cache contention) that a naive fixed
threshold would be flaky, failing pushes for noise rather than real
regressions. The gate instead computes, per benchmark, the median and
median absolute deviation (MAD, an outlier-resistant spread estimate)
across the last 10 `main`-branch results, ordered by the producing Git commit
timestamp encoded by each result filename, then flags a regression only when
the latest value exceeds `median + 6×MAD` (floored at 20% of the median, so
an unusually stable run of samples doesn't turn a trivial delta into a false
positive). Filesystem mtimes are deliberately not used for normal CI ordering:
history files are materialized from the automation branch during a run, so
their copy time is not their benchmark time. A benchmark needs at least 3
prior samples in the window before it's gated at all — new or rarely-run
benchmarks are reported as skipped, not failed, until enough history
accumulates.

If the first sample exceeds its noise-adjusted threshold, CI performs one
independent Criterion re-measurement in the same job and applies the same gate
again. Only a regression reproduced by that confirmation sample fails `main`.
The confirmation result becomes the canonical JSON persisted for that commit,
so a known first-pass outlier does not contaminate the rolling history.

This intentionally does not need a dedicated non-shared runner for the
coarse regression gate: the noise-adaptive threshold plus confirmation
measurement filters isolated shared-runner outliers without weakening the
regression threshold.

**Do not use cross-commit shared-runner deltas to choose or justify a runtime
optimization.** They are regression signals, not controlled A/B measurements.
Performance PRs should use the same-runner Nulang A/B workflow below as the
primary before/after evidence; a fixed self-hosted runner is preferred when
longitudinal absolute numbers matter.

## Cross-runtime Savina baselines

`benchmarks/cross_runtime/` contains an opt-in comparison harness for the
same counting, ping-pong, thread-ring, and fork-join workloads in Nulang,
Rust standard-library channels, Go channels/goroutines, and Erlang processes.
Use `scripts/cross_runtime_bench.py` to run every available runtime on the
same machine and emit one JSON report with exact toolchain metadata and median
timings.

Comparative runs default to `--cpu-mode single`: every measured runtime
process is pinned to the same one logical CPU, matching the current
single-shard Nulang Savina harness's compute budget. `--cpu-mode host` is
diagnostic only until a separate sharded Nulang fixture exists; in particular,
host-mode fork-join results must not be presented as a fair multicore
comparison. These are language/runtime baselines, not a universal framework
ranking; see the cross-runtime README for the full interpretation constraints.

## Same-runner Nulang A/B

`scripts/nulang_ab_bench.py` compares the current checkout against an exact
base ref on one host. Both variants are built before measurement, runs alternate
base/candidate order to reduce thermal/load drift bias, and measured processes
default to the same logical CPU. The report retains each workload's aggregate
median throughput for compatibility, but optimization evidence is also computed
from **round-aligned base/candidate pairs**. It reports the median paired
throughput/latency change, median speedup, and a deterministic percentile-
bootstrap 95% interval for the paired speedup. Pairing reduces the effect of
host-load and thermal drift that a ratio of two independent medians cannot
cancel.

The `Nulang actor A/B benchmarks` workflow uses the pull request's exact base
SHA rather than a moving branch name. This makes stacked performance PRs
incremental by construction: a child PR is measured against its parent stack
layer, while a root performance PR is measured against `main`.

PR runs use `--no-default-features --features native-codegen` to isolate the
core VM/JIT/AOT/actor runtime from unrelated optional integrations. Longer
manual runs can use the default feature set when production-profile validation
is needed.

Nulang-only A/B probes include a mailbox-only one-value admission lower bound,
a 0/1/4/5/16-value runtime enqueue sweep around the small-message inline
boundary, a matched warmed-bytecode/JIT vs AOT actor-drain comparison over the
same actor source, and first-run JIT-vs-interpreter crossover probes at 3k, 4k,
5k, and 7.5k loop trips. The actor comparison warms bytecode past the tier-up
threshold before timing and excludes enqueue time for both backends; it also
emits a `[backend-bench]` AOT speedup line. The tiering probes preconstruct
fresh VMs so their timed sections include interpreter execution and JIT
compilation/native execution, but not source compilation or VM/module setup.
The mailbox-vs-runtime pair is diagnostic: it isolates how much local-send cost
lives above message construction and mailbox admission before routing or
scheduler changes are attempted. The ordinary Nulang-only timings are emitted
as `[ab-bench]` records and are never folded into the cross-language
Rust/Go/Erlang comparison.

## Interpretation rules

Benchmark names describe the operation actually timed. Do not convert a result
into a broader throughput claim unless the timed body covers that broader
operation end to end.

Examples:
- `actor/message_enqueue/*` measures local mailbox admission/enqueue only.
- `actor/message_drain/*` measures scheduler + registered native-handler
  execution for messages already queued in setup.
- `actor/lifecycle_spawn_send_receive_gc` is an end-to-end lifecycle cost and
  is intentionally not a message-throughput benchmark.
- `gc/orca_throughput` exercises admitted primitive actor traffic plus the
  runtime's GC cadence; primitive values do not create ORCA foreign-reference
  bookkeeping.
- `gc/orca_foreign_ref_send/256` measures real local cross-actor pointer-send
  bookkeeping (message admission, foreign-count bump, coordinator submission,
  cycle-edge registration, and ready publication), excluding handler execution
  and pending-op draining.
- `dist/crdt_delta_compute` and `dist/gossip_membership_merge_4` are local
  algorithmic microbenchmarks, not network synchronization/convergence.
- `dist/nul0_actor_message_{encode,decode}/*` measures wire codec work only,
  excluding sockets, TLS, routing, queueing, and remote mailbox admission.
- `persist/checkpoint_json_{encode,decode}/*` measures real
  `ActorSnapshot` JSON codec work over logical payload sizes.

Setup that is not part of the operation under test should use Criterion
`iter_batched` (or equivalent) so runtime construction, fixture creation, and
preloading do not contaminate the timed body. Benchmark inputs and outputs must
be observable through `black_box` or a semantic assertion so the optimizer
cannot erase the work.

`docs/PERFORMANCE_ANALYSIS.md` should cite numbers from here, not estimates.
