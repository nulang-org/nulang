//! Nulang production benchmarks — criterion harness entry point.
//!
//! Run with: `cargo bench`
//! Individual group: `cargo bench --features numeric-bench --bench bench_main -- numeric_kernel`

mod actor_bench;
#[cfg(feature = "native-codegen")]
mod aot_bench;
mod cache_bench;
mod dist_bench;
mod gc_bench;
mod interp_bench;
#[cfg(feature = "native-codegen")]
mod jit_bench;
#[cfg(feature = "numeric-bench")]
mod numeric_bench;
mod persist_bench;
mod scheduler_bench;
mod vm_bench;

use criterion::criterion_main;

#[cfg(all(feature = "native-codegen", feature = "numeric-bench"))]
criterion_main!(
    vm_bench::benches,
    interp_bench::benches,
    actor_bench::benches,
    cache_bench::benches,
    aot_bench::benches,
    jit_bench::benches,
    numeric_bench::benches,
    gc_bench::benches,
    dist_bench::benches,
    persist_bench::benches,
    scheduler_bench::benches,
);

#[cfg(all(feature = "native-codegen", not(feature = "numeric-bench")))]
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

#[cfg(all(not(feature = "native-codegen"), feature = "numeric-bench"))]
criterion_main!(
    vm_bench::benches,
    interp_bench::benches,
    actor_bench::benches,
    cache_bench::benches,
    numeric_bench::benches,
    gc_bench::benches,
    dist_bench::benches,
    persist_bench::benches,
    scheduler_bench::benches,
);

#[cfg(all(not(feature = "native-codegen"), not(feature = "numeric-bench")))]
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
