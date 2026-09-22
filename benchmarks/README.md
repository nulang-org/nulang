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
across the last 10 `main`-branch results, then flags a regression only
when the latest value exceeds `median + 6×MAD` (floored at 20% of the
median, so an unusually stable run of samples doesn't turn a trivial
delta into a false positive). A benchmark needs at least 3 prior samples
in the window before it's gated at all — new or rarely-run benchmarks are
reported as skipped, not failed, until enough history accumulates.

This intentionally does not need a dedicated non-shared runner: the
noise-adaptive threshold is the fix, not the infrastructure change.

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
