//! Same-host comparator for Nulang's actor runtime versus BEAM.
//!
//! This intentionally mirrors `benchmarks/beam/beam_parity.escript` and emits
//! the same CSV schema so CI can compare the two runtimes without trying to
//! reconcile Criterion's statistical output with Erlang's one-shot timings.
//!
//! Usage:
//!   cargo run --release --example beam_parity -- 1000 10000

use nulang::runtime::{Actor, Runtime};
use nulang::vm::Value;
use std::time::Instant;

fn noop_handler(_actor: &mut Actor, _args: &[Value]) {}

fn spawn_noop_actor(rt: &mut Runtime) -> u64 {
    let actor = rt.spawn_actor(Box::new(|| vec![]));
    rt.actors
        .get_mut(&actor)
        .expect("fresh actor must be resident")
        .register_behavior("handle", noop_handler);
    actor
}

fn elapsed_us(start: Instant) -> u128 {
    start.elapsed().as_micros().max(1)
}

fn emit(name: &str, operations: usize, elapsed_us: u128) {
    let ops_per_sec = (operations as u128 * 1_000_000) / elapsed_us.max(1);
    println!("nulang,{name},{operations},{elapsed_us},{ops_per_sec}");
}

fn bench_spawn_idle(n: usize) {
    let mut rt = Runtime::new();
    let start = Instant::now();
    for _ in 0..n {
        rt.spawn_actor(Box::new(|| vec![]));
    }
    emit("spawn_idle", n, elapsed_us(start));
}

fn bench_single_mailbox_flood(n: usize) {
    let mut rt = Runtime::new();
    let actor = spawn_noop_actor(&mut rt);
    rt.run_scheduler();

    let message = Value::int(1);
    let start = Instant::now();
    for _ in 0..n {
        rt.send_message(actor, "handle", &[message]);
    }
    rt.run_scheduler();
    rt.process_gc_ops();
    emit("single_mailbox_flood", n, elapsed_us(start));
}

fn bench_fanout_one_message_each(n: usize) {
    let mut rt = Runtime::new();
    let actors = (0..n)
        .map(|_| spawn_noop_actor(&mut rt))
        .collect::<Vec<_>>();
    rt.run_scheduler();

    let message = Value::int(1);
    let start = Instant::now();
    for actor in actors {
        rt.send_message(actor, "handle", &[message]);
    }
    rt.run_scheduler();
    rt.process_gc_ops();
    emit("fanout_one_message_each", n, elapsed_us(start));
}

fn main() {
    let counts = {
        let parsed = std::env::args()
            .skip(1)
            .map(|arg| arg.parse::<usize>().expect("counts must be positive integers"))
            .collect::<Vec<_>>();
        if parsed.is_empty() {
            vec![1_000, 10_000]
        } else {
            parsed
        }
    };

    assert!(counts.iter().all(|count| *count > 0), "counts must be positive");
    println!("runtime,benchmark,operations,elapsed_us,ops_per_sec");
    for count in counts {
        bench_spawn_idle(count);
        bench_single_mailbox_flood(count);
        bench_fanout_one_message_each(count);
    }
}
