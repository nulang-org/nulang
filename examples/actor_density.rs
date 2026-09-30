//! Manual actor-density and spawn-cost probe.
//!
//! This is intentionally not part of the default Criterion suite. Density
//! measurements need one long-lived process on a controlled host; repeating
//! 100k-1M actor allocations inside shared CI would add noise and cost without
//! producing publication-quality evidence.
//!
//! Examples:
//!   cargo run --release --example actor_density -- idle 10000
//!   cargo run --release --example actor_density -- idle 100000
//!   cargo run --release --example actor_density -- construct 100000
//!   cargo run --release --example actor_density -- fanout 100000
//!
//! Run Nulang and any BEAM comparator on the same otherwise-idle host.

use nulang::runtime::{Actor, ActorHeap, FlightRecorder, Mailbox, OrcaGc, Runtime, TraceEntry};
use nulang::vm::Value;
use std::hint::black_box;
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

fn print_layout() {
    println!("actor_struct_bytes={}", size_of::<Actor>());
    println!("mailbox_struct_bytes={}", size_of::<Mailbox>());
    println!("actor_heap_struct_bytes={}", size_of::<ActorHeap>());
    println!("orca_gc_struct_bytes={}", size_of::<OrcaGc>());
    println!(
        "flight_recorder_struct_bytes={}",
        size_of::<FlightRecorder>()
    );
    println!("trace_entry_struct_bytes={}", size_of::<TraceEntry>());
}

fn print_rss(name: &str, value: Option<u64>) {
    match value {
        Some(bytes) => println!("{name}={bytes}"),
        None => println!("{name}=unavailable"),
    }
}

fn print_rate(prefix: &str, count: usize, elapsed: std::time::Duration) {
    let seconds = elapsed.as_secs_f64();
    let rate = count as f64 / seconds.max(f64::EPSILON);
    let ns_per = elapsed.as_nanos() as f64 / count.max(1) as f64;
    println!("{prefix}_seconds={seconds:.6}");
    println!("{prefix}_per_second={rate:.0}");
    println!("{prefix}_ns_per_item={ns_per:.1}");
}

fn run_construct_only(actor_count: usize) {
    let rss_before = resident_bytes();
    let start = Instant::now();
    let mut actors = Vec::with_capacity(actor_count);
    for idx in 0..actor_count {
        // Keep the same synthetic-name shape used by Runtime::spawn_actor so
        // String allocation remains represented in this constructor baseline.
        let id = idx as u64 + 1;
        actors.push(Actor::new(id, format!("actor_{id}"), 0));
    }
    let elapsed = start.elapsed();
    let rss_after = resident_bytes();

    black_box(&actors);
    println!("mode=construct");
    println!("actor_count={actor_count}");
    print_rate("construct", actor_count, elapsed);
    print_rss("rss_before_bytes", rss_before);
    print_rss("rss_after_construct_bytes", rss_after);
    match (rss_before, rss_after) {
        (Some(before), Some(after)) => {
            let delta = after.saturating_sub(before);
            println!("rss_construct_delta_bytes={delta}");
            println!(
                "rss_construct_delta_bytes_per_actor={:.1}",
                delta as f64 / actor_count as f64
            );
        }
        _ => println!("rss_construct_delta_bytes_per_actor=unavailable"),
    }
}

fn spawn_idle_runtime(actor_count: usize, register_behavior: bool) -> (Runtime, Vec<u64>) {
    let mut runtime = Runtime::new();
    let mut ids = Vec::with_capacity(actor_count);
    for _ in 0..actor_count {
        let id = runtime.spawn_actor(Box::new(Vec::new));
        if register_behavior {
            runtime
                .actors
                .get_mut(&id)
                .expect("new actor must be resident")
                .register_behavior("handle", noop_handler);
        }
        ids.push(id);
    }
    (runtime, ids)
}

fn run_idle(actor_count: usize) {
    let rss_before = resident_bytes();

    let start = Instant::now();
    let (mut runtime, ids) = spawn_idle_runtime(actor_count, false);
    let spawn_elapsed = start.elapsed();
    let rss_after_spawn = resident_bytes();

    let settle_start = Instant::now();
    runtime.run_scheduler();
    let settle_elapsed = settle_start.elapsed();
    let rss_after_settle = resident_bytes();

    black_box(&ids);
    black_box(&runtime);

    let actor_heap_used_bytes: usize = runtime
        .actors
        .values()
        .map(|actor| actor.heap.used())
        .sum();

    println!("mode=idle");
    println!("actor_count={actor_count}");
    print_rate("spawn", actor_count, spawn_elapsed);
    println!(
        "scheduler_settle_seconds={:.6}",
        settle_elapsed.as_secs_f64()
    );
    println!("actor_heap_used_bytes={actor_heap_used_bytes}");
    println!(
        "actor_heap_used_bytes_per_actor={:.1}",
        actor_heap_used_bytes as f64 / actor_count as f64
    );

    print_rss("rss_before_bytes", rss_before);
    print_rss("rss_after_spawn_bytes", rss_after_spawn);
    print_rss("rss_after_settle_bytes", rss_after_settle);
    match (rss_before, rss_after_spawn) {
        (Some(before), Some(after)) => {
            let delta = after.saturating_sub(before);
            println!("rss_spawn_delta_bytes={delta}");
            println!(
                "rss_spawn_delta_bytes_per_actor={:.1}",
                delta as f64 / actor_count as f64
            );
        }
        _ => println!("rss_spawn_delta_bytes_per_actor=unavailable"),
    }
}

fn noop_handler(_actor: &mut Actor, _args: &[Value]) {}

fn run_fanout(actor_count: usize) {
    let (mut runtime, ids) = spawn_idle_runtime(actor_count, true);
    runtime.run_scheduler();

    let start = Instant::now();
    for &id in &ids {
        runtime.send_message_by_id(id, 0, &[]);
    }
    runtime.run_scheduler();
    runtime.process_gc_ops();
    let elapsed = start.elapsed();

    for &id in &ids {
        assert!(
            runtime
                .actors
                .get(&id)
                .expect("actor must remain resident")
                .mailbox
                .is_empty(),
            "fanout must drain every mailbox"
        );
    }

    println!("mode=fanout");
    println!("actor_count={actor_count}");
    print_rate("fanout_send_and_drain", actor_count, elapsed);
}

fn usage(program: &str) -> ! {
    eprintln!(
        "usage: {program} <idle|construct|fanout> [actor_count]\n\n\
         examples:\n  {program} idle 100000\n  {program} construct 100000\n  {program} fanout 100000"
    );
    std::process::exit(2);
}

fn main() {
    print_layout();

    let mut args = std::env::args();
    let program = args.next().unwrap_or_else(|| "actor_density".to_string());
    let mode = args.next().unwrap_or_else(|| "idle".to_string());
    let actor_count = args
        .next()
        .map(|arg| {
            arg.parse::<usize>()
                .expect("actor_count must be a positive integer")
        })
        .unwrap_or(10_000);
    if actor_count == 0 || args.next().is_some() {
        usage(&program);
    }

    match mode.as_str() {
        "idle" => run_idle(actor_count),
        "construct" => run_construct_only(actor_count),
        "fanout" => run_fanout(actor_count),
        _ => usage(&program),
    }
}
