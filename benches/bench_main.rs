//! Nulang production benchmarks — criterion harness entry point.
//!
//! Run with: `cargo bench`
//! Individual group: `cargo bench --bench bench_main -- vm_throughput`

mod actor_bench;
mod aot_bench;
mod dist_bench;
mod gc_bench;
mod interp_bench;
mod jit_bench;
mod persist_bench;
mod runtime_hot_path_bench;
mod scheduler_bench;
mod vm_bench;

use criterion::criterion_main;

criterion_main!(
    vm_bench::benches,
    interp_bench::benches,
    actor_bench::benches,
    aot_bench::benches,
    jit_bench::benches,
    gc_bench::benches,
    dist_bench::benches,
    persist_bench::benches,
    runtime_hot_path_bench::benches,
    scheduler_bench::benches,
);
