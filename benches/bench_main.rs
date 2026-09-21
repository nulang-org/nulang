//! Nulang production benchmarks — criterion harness entry point.
//!
//! Run with: `cargo bench`
//! Individual group: `cargo bench --bench bench_main -- vm_throughput`

mod actor_bench;
#[cfg(feature = "native-aot")]
mod aot_bench;
mod cache_bench;
mod dist_bench;
mod gc_bench;
mod interp_bench;
#[cfg(feature = "jit-codegen")]
mod jit_bench;
mod persist_bench;
mod scheduler_bench;
mod vm_bench;

use criterion::criterion_main;

#[cfg(all(feature = "jit-codegen", feature = "native-aot"))]
criterion_main!(
    vm_bench::benches,
    interp_bench::benches,
    actor_bench::benches,
    cache_bench::benches,
    aot_bench::benches,
    jit_bench::benches,
    gc_bench::benches,
    dist_bench::benches,
    persist_bench::benches,
    scheduler_bench::benches,
);

#[cfg(all(feature = "jit-codegen", not(feature = "native-aot")))]
criterion_main!(
    vm_bench::benches,
    interp_bench::benches,
    actor_bench::benches,
    cache_bench::benches,
    jit_bench::benches,
    gc_bench::benches,
    dist_bench::benches,
    persist_bench::benches,
    scheduler_bench::benches,
);

#[cfg(all(not(feature = "jit-codegen"), feature = "native-aot"))]
criterion_main!(
    vm_bench::benches,
    interp_bench::benches,
    actor_bench::benches,
    cache_bench::benches,
    aot_bench::benches,
    gc_bench::benches,
    dist_bench::benches,
    persist_bench::benches,
    scheduler_bench::benches,
);

#[cfg(all(not(feature = "jit-codegen"), not(feature = "native-aot")))]
criterion_main!(
    vm_bench::benches,
    interp_bench::benches,
    actor_bench::benches,
    cache_bench::benches,
    gc_bench::benches,
    dist_bench::benches,
    persist_bench::benches,
    scheduler_bench::benches,
);
