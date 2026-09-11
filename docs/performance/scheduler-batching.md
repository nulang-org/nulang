# Scheduler batching validation

This note defines the validation criteria for the priority-aware global-to-local scheduler batching change.

## Invariants

The optimization is acceptable only if all of these remain true:

1. `High` work always runs before available `Normal` and `Low` work.
2. `Normal` work always runs before available `Low` work.
3. A task transferred by `steal_batch_and_pop` is executed exactly once.
4. The live runtime owner path (`Scheduler::dequeue`) does not probe unused peer queues.
5. Generic multi-worker callers (`Scheduler::next_task`) retain peer work stealing.
6. Scheduler backoff never dequeues and discards work.

Unit tests in `src/runtime/scheduler.rs` cover these behavioral invariants.

## Performance experiment

Benchmark both the base branch and this branch with the same release build and CPU affinity.

Workloads:

- 1,000 Normal-priority actors enqueued globally and drained by `dequeue()`.
- 100,000 Normal-priority actors enqueued globally and drained by `dequeue()`.
- Mixed 10/80/10 High/Normal/Low actors to validate priority overhead.
- Burst workload: enqueue 10,000 actors, drain, repeat 100 times.
- True multi-worker synthetic workload through `next_task(worker_idx)` to ensure peer stealing does not regress materially.

Record:

- actors dispatched / second
- p50 / p95 / p99 dispatch latency
- `tasks_from_global_queue`
- `tasks_from_local_queue`
- `steal_attempts`
- CPU cycles and cache misses when `perf stat` is available

Run each workload at least three times on both revisions and compare medians, not a single run.

Expected signal on the current one-owner runtime path:

- after the first global batch transfer, most remaining tasks should be counted as local-queue work;
- `dequeue()` should report zero peer steal attempts;
- throughput should improve primarily under burst/high-contention global enqueue workloads.

Acceptance criteria:

- no correctness or priority-invariant regression;
- no material (>2%) median throughput regression in mixed-priority or synthetic multi-worker workloads;
- a measurable reduction in shared-global dispatches and peer-steal probes on the one-owner runtime path;
- keep the change only if the measured benefit justifies the extra per-priority local queues.

Do not claim a production speedup from this change until release-mode measurements show one.