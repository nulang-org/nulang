# Profile Nulang non-durable actor hot paths

Use this **diagnostic** when cross-runtime Savina comparisons show a large
difference, but the runtime responsible for the overhead is not yet known.
It deliberately uses the existing Nulang Savina runner, not a new actor
implementation.

## Unprivileged phase attribution in CI

The profiler workflow also runs **real optimized Nulang workloads** with
`--phase-profile` (counting and fork-join). It records three complete
rounds per workload in `nulang-savina-phase-timings-<commit>` as JSONL
and a median summary. Each JSONL row checks the same completed logical
message count as the ordinary suite and partitions wall-clock time into:

- `phase_admission_ns`: application-to-runtime enqueue before scheduler start
- `phase_scheduler_ns`: `Runtime::run_scheduler()` through completed delivery

The two phase durations must add up exactly to `elapsed_ns`. Source
compilation, actor creation, and initial wire-up are **excluded from the
timed sections**, although the benchmark process still performs them.

This is a wall-clock **coarse component breakdown**, not CPU sampling and
not an instruction-level flame graph. It cannot attribute time *within*
`run_scheduler` to VM, timers, GC, scheduler selection, etc. The CLI
option is opt-in; the ordinary cross-runtime output retains its existing
record schema and benchmark semantics.

Run manually with:

```bash
cargo run --locked --profile savina --no-default-features \
  --features savina-bench --bin nulang-savina -- \
  --benchmark counting --repeat 5 --phase-profile --format jsonl
cargo run --locked --profile savina --no-default-features \
  --features savina-bench --bin nulang-savina -- \
  --benchmark fork_join --repeat 5 --phase-profile --format jsonl
```

## Prerequisites

Linux on an otherwise idle host, `perf`, Rust/Cargo, and permission to use
performance counters (`kernel.perf_event_paranoid` must allow your user to
record the process). Do not change production kernel security policy just to
get a benchmark result.

## Run

```bash
python3 -m unittest discover -s tests -p test_profile_stateless_actors.py -v

python3 scripts/profile_stateless_actors.py \
  --workload ping_pong --repeat 25 --frequency 99 \
  --cpu-mode single --output-dir /tmp/nulang-profile-ping
python3 scripts/profile_stateless_actors.py \
  --workload fork_join --repeat 25 --frequency 99 \
  --cpu-mode single --output-dir /tmp/nulang-profile-forkjoin
```

Artifacts:

- `perf.data`: raw sampling evidence; use `perf report -i perf.data` to
  interactively inspect flame stacks.
- `perf-report.txt`: recorded-symbol top list, 0.5% minimum share.
- `samples.jsonl`: semantic-success records emitted by the real Savina
  runner; missing, duplicate, invalid, or wrong-workload records fail.
- `manifest.json`: exact git SHA, toolchains, CPU affinity, workload,
  repetition count, profiling options and build settings.
- `perf.stderr.txt`: permission failures and perf-record diagnostics.

This runner compiles Nulang with `--locked --profile savina
--no-default-features --features savina-bench` plus debug info and forced
frame pointers. Consequently it profiles **bytecode actor execution** and
scheduler/mailbox overhead, not JIT or AOT actor execution.

For `ping_pong`, the profiler uses the new opt-in `--reuse-setup` mode:
source compilation, runtime creation, actor spawn and wiring occur exactly
once before repeated message rounds. The pong counter is checked against
`20,000 × iteration` after every round to catch lost, duplicated, or
incomplete actor-message execution. The cross-runtime benchmark runner
**does not use this mode**, preserving its original semantics. Other
workloads still reconstruct their fixtures on every iteration, so their CPU
samples include repeated compilation and cannot be interpreted as runtime-
only percentages.

**Important limitations:**

1. Linux perf statistically samples the entire process, including initial
   setup and source compilation, not only the timed scheduler region. Ping-pong
   compiles only once; use sufficient repetitions to amortize that setup.
   Other workloads still compile on each round. Inspect CPU stacks
   qualitatively, not as exact percentages of `run_scheduler`.
2. Debug symbols, forced frame pointers, perf sampling and single-CPU affinity
   can change throughput. Never compare these profiler timings against normal
   optimized builds or use them in performance regression gates.
3. For an optimization candidate, use `scripts/nulang_ab_bench.py` against its
   **exact parent SHA**, ideally on the same controlled host. Only retain a
   runtime optimization that improves confirmed messaging latency/throughput
   without compromising fairness, isolation, selective receive, or GC.

Prioritize evidence from `Runtime::run_scheduler`,
`Runtime::step_actor`, `Mailbox::pop`, `Runtime::send_message_by_id`, and
scheduler dispatch before restructuring any of those hot paths. A cross-runtime
throughput gap alone does not identify a bottleneck.
