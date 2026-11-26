//! Timer deadline lookup microbenchmarks.
//!
//! Isolates heap deadline lookup from timer insertion and expiry delivery.
//! The active-root case measures the usual scheduler sleep decision.
//! The cancelled-root case keeps the fallback path observable.
//!
//! Run with: cargo bench --bench bench_main -- timer/next_deadline

use criterion::{black_box, criterion_group, BenchmarkId, Criterion};
use nulang::runtime::TimerWheel;
use std::time::{Duration, Instant};

fn populated_wheel(count: usize, cancel_earliest: bool) -> TimerWheel {
    let wheel = TimerWheel::new();
    let now = Instant::now();
    for offset in 1..=count {
        let id = wheel.timer_sleep_wake_at(now, Duration::from_millis(offset as u64), 42);
        if offset == 1 && cancel_earliest {
            assert!(wheel.cancel(id));
        }
    }
    wheel
}

fn bench_next_deadline(c: &mut Criterion) {
    let mut group = c.benchmark_group("timer/next_deadline");

    for count in [64usize, 1024, 16_384] {
        let active = populated_wheel(count, false);
        group.bench_with_input(
            BenchmarkId::new("active_root", count),
            &active,
            |b, wheel| b.iter(|| black_box(wheel.next_deadline())),
        );

        let cancelled = populated_wheel(count, true);
        group.bench_with_input(
            BenchmarkId::new("cancelled_root", count),
            &cancelled,
            |b, wheel| b.iter(|| black_box(wheel.next_deadline())),
        );
    }

    group.finish();
}

criterion_group!(benches, bench_next_deadline);
