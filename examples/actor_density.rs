//! Manual actor-density probe for BEAM-parity work.
//!
//! This is intentionally not a Criterion benchmark: resident-memory tests need
//! a single long-lived process and controlled host conditions rather than many
//! sampled iterations.
//!
//! Run in release mode, ideally on an otherwise idle Linux host:
//!
//! ```text
//! cargo run --release --example actor_density -- 10000
//! cargo run --release --example actor_density -- 100000
//! cargo run --release --example actor_density -- 1000000
//! ```

use nulang::runtime::{Actor, ActorHeap, FlightRecorder, Mailbox, OrcaGc, Runtime, TraceEntry};
use std::mem::size_of;
use std::time::Instant;

#[cfg(target_os = "linux")]
fn resident_bytes() -> Option<u64> {
    let status = std::fs::read_to_string("/proc/self/status").ok()?;
    let line = status.lines().find(|line| line.starts_with("VmRSS:"))?;
    let kib = line.split_whitespace().nth(1)?.parse::<u64>().ok()?;
    kib.checked_mul(1024)
}

#[cfg(not(target_os = "linux"))]
fn resident_bytes() -> Option<u64> {
    None
}

fn main() {
    let actor_count = std::env::args()
        .nth(1)
        .map(|arg| {
            arg.parse::<usize>()
                .expect("actor count must be a positive integer")
        })
        .unwrap_or(10_000);
    assert!(actor_count > 0, "actor count must be greater than zero");

    println!("actor_struct_bytes={}", size_of::<Actor>());
    println!("mailbox_struct_bytes={}", size_of::<Mailbox>());
    println!("actor_heap_struct_bytes={}", size_of::<ActorHeap>());
    println!("orca_gc_struct_bytes={}", size_of::<OrcaGc>());
    println!(
        "flight_recorder_struct_bytes={}",
        size_of::<FlightRecorder>()
    );
    println!("trace_entry_struct_bytes={}", size_of::<TraceEntry>());

    let mut runtime = Runtime::new();
    let rss_before = resident_bytes();

    let spawn_start = Instant::now();
    for _ in 0..actor_count {
        runtime.spawn_actor(Box::new(|| vec![]));
    }
    let spawn_elapsed = spawn_start.elapsed();
    let rss_after_spawn = resident_bytes();

    let settle_start = Instant::now();
    runtime.run_scheduler();
    let settle_elapsed = settle_start.elapsed();
    let rss_after_settle = resident_bytes();

    let actor_heap_capacity_bytes: usize = runtime
        .actors
        .values()
        .map(|actor| actor.heap.used() + actor.heap.free_bytes())
        .sum();

    let actors_per_second = actor_count as f64 / spawn_elapsed.as_secs_f64().max(f64::EPSILON);
    let rss_delta_bytes = match (rss_before, rss_after_spawn) {
        (Some(before), Some(after)) => Some(after.saturating_sub(before)),
        _ => None,
    };
    let rss_bytes_per_actor = rss_delta_bytes.map(|bytes| bytes as f64 / actor_count as f64);

    println!("actor_count={actor_count}");
    println!("spawn_seconds={:.6}", spawn_elapsed.as_secs_f64());
    println!("actors_per_second={actors_per_second:.0}");
    println!(
        "scheduler_settle_seconds={:.6}",
        settle_elapsed.as_secs_f64()
    );
    println!("initial_actor_heap_capacity_bytes={actor_heap_capacity_bytes}");
    println!(
        "initial_actor_heap_capacity_bytes_per_actor={:.1}",
        actor_heap_capacity_bytes as f64 / actor_count as f64
    );

    match rss_before {
        Some(bytes) => println!("rss_before_bytes={bytes}"),
        None => println!("rss_before_bytes=unavailable"),
    }
    match rss_after_spawn {
        Some(bytes) => println!("rss_after_spawn_bytes={bytes}"),
        None => println!("rss_after_spawn_bytes=unavailable"),
    }
    match rss_after_settle {
        Some(bytes) => println!("rss_after_settle_bytes={bytes}"),
        None => println!("rss_after_settle_bytes=unavailable"),
    }
    match rss_delta_bytes {
        Some(bytes) => println!("rss_spawn_delta_bytes={bytes}"),
        None => println!("rss_spawn_delta_bytes=unavailable"),
    }
    match rss_bytes_per_actor {
        Some(bytes) => println!("rss_spawn_delta_bytes_per_actor={bytes:.1}"),
        None => println!("rss_spawn_delta_bytes_per_actor=unavailable"),
    }
}
