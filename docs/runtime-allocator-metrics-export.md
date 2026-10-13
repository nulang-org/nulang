# Runtime allocator metrics export

The runtime exports read-only allocation snapshots through its existing
`MetricsSnapshot` JSON, Prometheus `/metrics` endpoint, and optional OTLP
metrics exporter. No new listener, allocation sampler, or background thread
is introduced.

## Metrics

| Prometheus gauge | OpenTelemetry gauge | Meaning |
| --- | --- | --- |
| `nulang_allocator_heap_allocations_since_reset` | `nulang.allocator.heap.allocations_since_reset` | Successful allocation calls since each active heap's last reset, summed |
| `nulang_allocator_heap_requested_payload_bytes_since_reset` | `nulang.allocator.heap.requested_payload_bytes_since_reset` | Requested payload bytes successfully served by the same heaps, summed |
| `nulang_allocator_heap_live_objects` | `nulang.allocator.heap.live_objects` | Current objects in the live lists of those heaps |
| `nulang_allocator_arena_epoch_allocations` | `nulang.allocator.arena.epoch_allocations` | Successful allocations served in each live actor's current arena epoch, summed |

**Scope:** Heap gauges include `Runtime::main_heap` and the heaps owned by
currently live actors. Arena epoch allocations include only arenas owned by
currently live actors. The actor pool, exited/retired heaps retained for
foreign references, and standalone VMs not owned by this `Runtime` are
excluded. There are no per-actor labels, avoiding unbounded cardinality.

**Gauge semantics are intentional.** Removal or recycling of an actor and
arena/heap resets can decrease these values. They cannot be used as lifetime
process allocation counters or directly differentiated into a true allocation
rate with `rate()`. Do not assume the heap and arena totals describe the
same time interval. Current-epoch arena allocations exclude prior completed
epochs. Requested payload bytes exclude object headers, size-class padding,
retained arena capacity, native allocation overhead, and process RSS.

Runtime snapshots are sampled when `metrics_snapshot` executes. An endpoint
that has not been published recently may return an older snapshot; this work
does not change the publishing cadence.

## Validation

```sh
cargo test --test runtime_allocation_counters
cargo test --test runtime_allocator_metrics
cargo test
cargo fmt --all -- --check
```

The allocator-counters PR must be merged before the stacked metrics PR, or
the latter must be rebased and validated against the final parent revision.

For performance baselines, measure request throughput and latency with the
metrics exporter both enabled and disabled, recording the same workload and
revision. Do not claim lower memory consumption solely from these metrics.
