# Experimental compact ORCA object header

Status: experimental and **off by default**. The 56-byte ORCA header remains
Nulang's standard memory ABI. This proposal adds an opt-in
`compact-orca-header` Cargo feature for evaluating a 48-byte object header,
without changing language source syntax or the public ownership model.

## Representation

The current header stores `ref_count`, `foreign_count`, `sticky`,
`size_class`, `gc_color`, `type_tag`, `actor_id`, `size`,
`payload_size`, `live_next`, and `live_prev`. Under the experimental
feature only, the redundant `size` word is omitted. The total allocation
extent is derived from `HEADER_SIZE + align_up(payload_size, 8)`, while the
exact requested length remains in `payload_size`. This preserves byte-accurate
string lengths, size-class classification, pointer stability, and LOS
deallocation layouts.

The JIT's direct header load for array bounds must use `payload_size`
instead of the removed `size` field; array slots are eight-byte values.
The AOT, callbacks, arena, heap iteration, and LOS paths use common
`OrcaHeader` extent accessors. The default feature configuration preserves
the old field layout and offsets.

### Modeled space per small object

These values are **analytical size-class allocations, not RSS measurements**.
The allocator rounds *header plus aligned payload* to the next class.

| Requested payload | Default 56-byte header | Experimental 48-byte header |
|---:|---:|---:|
| 8 B | 64 B | 64 B |
| 16 B | 128 B | 64 B |
| 32 B | 128 B | 128 B |
| 64 B | 128 B | 128 B |
| 128 B | 256 B | 256 B |

The 16-byte case may use half as much bump-allocated space per object,
but other payload sizes receive no benefit from the current class thresholds.
Actual peak RSS and latency impacts depend on the allocation histogram,
heap reuse, objects surviving to retirement, and instruction-level costs.

## Validation before considering promotion

```sh
cargo fmt --all -- --check
cargo test --locked --no-default-features --features compact-orca-header --lib
cargo test --locked --features compact-orca-header --lib
cargo clippy --locked --all-targets --all-features -- -D clippy::correctness
cargo bench --bench bench_main -- gc/actor_heap_small_alloc
cargo bench --bench bench_main -- gc/orca_local_alloc_drop
```

The benchmarks are in the independent baseline proposal, PR #1549.
Run paired exact-base/candidate benchmarks on the same host, for both feature
profiles, with repeated warmups and confidence intervals. Capture peak RSS,
allocations and reclaimed bytes, throughput, and p95/p99 latency. Include
16-byte as well as mixed-size object distributions, real actor messages,
crash/restart/retired-heaps, cycle detection, strings, FFI, serialized durable
continuations, JIT, AOT, and interpreter parity.

**Safety gate:** do not enable the feature by default or merge this experiment
until full CI and sanitizer/Miri coverage where available demonstrate no
invalid free, stale pointer, or cross-actor premature release. The header
is an internal runtime ABI, but all consumers of raw payload pointers,
including external embedders, must be audited before promotion.

The collector and cross-actor hold protocol remain ORCA; this is an
independent allocation-layout optimization, not a replacement for GC or a
Perceus-style ownership analysis.
