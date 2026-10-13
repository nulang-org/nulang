# Runtime allocation counters (allocator operations, not RSS)

Nulang already maintains allocation/free counters in each actor heap and
allocation/reset counters in the optional per-activation iso arena. The
`allocation_snapshot()` APIs make those counters observable and add
**successful requested payload bytes** in both allocators.

The counters are based on successful allocator operations, not a static
compiler estimate. A successful allocation served from a free list is counted
again as a new allocation operation. Requested bytes exclude ORCA headers,
size-class padding, reserved capacity, operating-system allocation events,
GC overhead, and process RSS.

## Rust embedder usage

```rust
let stats = actor.allocation_snapshot();
let json = serde_json::to_string_pretty(&stats)?;
```

`ActorAllocationSnapshot` contains `actor_id`, `heap`, and `arena`
subobjects. An embedder can also call `ActorHeap::allocation_snapshot()` and
`IsoArena::allocation_snapshot()` directly.

### Counter lifetimes

- **Heap**: `allocations`, `frees`, `requested_payload_bytes`, and
  `live_objects` reset on `ActorHeap::reset()`. `committed_bump_bytes` and
  `peak_committed_bump_bytes` account for bump-block bytes only; they do not
  include the large-object space.
- **Arena**: `allocations` and `requested_payload_bytes` describe the
  lifetime of the *arena object*, including reused blocks and multiple
  activation epochs. `epoch_allocations` resets on `IsoArena::reset()`,
  while `resets` increments.
- **Actor pooling**: a pooled `Actor` can receive a new actor ID after
  `Actor::reset()`. Its heap snapshot starts anew, but its arena lifetime
  counters deliberately persist. **Do not sum the heap and arena lifetime
  totals or attribute them to one actor ID across reuse.**

These APIs are read-only snapshots. They do not allocate VM values, run code,
change pointer ownership, or alter ORCA collection. No new background
monitoring threads or network listeners are introduced.

## Allocation baseline benchmarks

The existing Criterion harness (`benches/bench_main.rs`) registers
`benches/gc_bench.rs`. It now also registers two previously defined but
unreachable allocation benchmark groups:

- `gc/actor_heap_small_alloc`: 512 successful fresh heap allocations per
  sample, at payload sizes 8, 16, 32, 64, and 128 bytes.
- `gc/orca_local_alloc_drop`: 512 successful ORCA allocations and
  immediate last-reference drops per sample, at the same payload sizes.

Use the existing supported harness:

```sh
cargo bench --locked --bench bench_main -- gc/actor_heap_small_alloc
cargo bench --locked --bench bench_main -- gc/orca_local_alloc_drop
cargo bench --locked --bench bench_main -- gc/orca_foreign_ref_send
python3 -m unittest scripts/tests/test_benchmark_timing_contract.py
```

All three GC benchmarks use `iter_batched_ref` so setup and owned fixture
destruction occur **outside** Criterion's measured closure. Allocation, drops,
and pointer bookkeeping remain measured. The GC benchmark group is exercised
by the existing `cargo bench --locked --bench bench_main` command used
by CI on `main`. For a parent/candidate comparison, run the same filter,
payload and number of samples under controlled CPU conditions; the
regression checker is informational for a new benchmark until it has enough
historical samples.

These are **benchmarks prepared for execution**, not published measured
results. Do not claim that byte counting is negligible before a paired
parent-vs-candidate run.

## Validation

```sh
cargo test --test runtime_allocation_counters
cargo test
cargo fmt --all -- --check
```

Benchmark before and after instrumentation using allocation-heavy workloads:
the extra successful-allocation byte counter operates on the allocator hot
paths. Report any measurable throughput regression before promotion.

The preceding static report PR (`nulang explain allocations <file.nbc>`) gives
**eligibility predictions**. These runtime counters give **observed allocator
requests**. Neither provides whole-process physical memory usage, automatic
per-function attribution, full GC retain/release tracing, or bytes retained.

A future attribution pass should label measurements by workload/actor
activation and include effects, closure construction, and native code paths
only when their allocation sites can be identified unambiguously.
