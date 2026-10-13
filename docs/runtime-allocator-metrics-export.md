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

### Existing ORCA reference traffic

The same runtime snapshot now includes `orca_traffic`. These are views of
**existing** `OrcaGc::stats()` counters, not newly instrumented reference
operations.

| Prometheus gauge | OpenTelemetry gauge | Meaning |
| --- | --- | --- |
| `nulang_orca_local_refs_created_active` | `nulang.orca.local_refs_created.active` | Local references created by active ORCA GC engines |
| `nulang_orca_local_refs_dropped_active` | `nulang.orca.local_refs_dropped.active` | Local references dropped by active ORCA GC engines |
| `nulang_orca_foreign_refs_sent_active` | `nulang.orca.foreign_refs_sent.active` | Foreign send operations recorded by active ORCA GC engines |
| `nulang_orca_foreign_refs_received_active` | `nulang.orca.foreign_refs_received.active` | Foreign receive operations recorded by active ORCA GC engines |

ORCA gauges include the runtime's `main_gc` and live actors' GC engines,
excluding pooled actors and GC instances destroyed on exit. They can fall
as actors exit, restart, or get pooled, so **do not use `rate()` on them**.
These counters do not include direct header changes bypassing `OrcaGc`,
standalone VMs, or destroyed actor history. They do not distinguish
unnecessary retains from required retains and therefore are not proof of an
optimization opportunity. The correct next step is a controlled workload
comparison, followed by MIR-level ownership analysis.

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
cargo test --test runtime_orca_traffic_metrics
cargo test
cargo fmt --all -- --check
```

The allocator-counters PR must be merged before the stacked metrics PR, or
the latter must be rebased and validated against the final parent revision.

For performance baselines, measure request throughput and latency with the
metrics exporter both enabled and disabled, recording the same workload and
revision. Do not claim lower memory consumption solely from these metrics.
