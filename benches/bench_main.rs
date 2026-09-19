//! Nulang production benchmarks — criterion harness entry point.
//!
//! Run with: `cargo bench`
//! Individual group: `cargo bench --bench bench_main -- vm_throughput`

mod actor_bench;
mod cache_bench;
#[cfg(feature = "native-codegen")]
mod aot_bench;
mod dist_bench;
mod gc_bench;
mod interp_bench;
#[cfg(feature = "native-codegen")]
mod jit_bench;
mod persist_bench;
mod scheduler_bench;
mod vm_bench;

use criterion::criterion_main;

#[cfg(feature = "native-codegen")]
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

#[cfg(not(feature = "native-codegen"))]
criterion_main!(
    vm_bench::benches,
    interp_bench::benches,
    actor_bench::benches,
    gc_bench::benches,
    dist_bench::benches,
    persist_bench::benches,
    scheduler_bench::benches,
);
