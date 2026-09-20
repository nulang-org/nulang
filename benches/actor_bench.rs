//! Actor throughput and scaling benchmarks.

use criterion::{black_box, criterion_group, Criterion};
use nulang::runtime::Runtime;
use nulang::vm::Value;

fn bench_spawn_send_receive(c: &mut Criterion) {
    c.bench_function("actor/spawn_send_receive", |b| {
        b.iter(|| {
            let mut rt = Runtime::new();
            let actor_id = rt.spawn_actor(Box::new(|| vec![]));
            let msg = Value::int(42);
            rt.send_message(actor_id, "handle", &[msg]);
            for _ in 0..20 {
                rt.run_scheduler();
                rt.process_gc_ops();
            }
            black_box(actor_id);
        })
    });
}

fn bench_message_throughput(c: &mut Criterion) {
    c.bench_function("actor/message_throughput", |b| {
        b.iter(|| {
            let mut rt = Runtime::new();
            let consumer = rt.spawn_actor(Box::new(|| vec![]));
            let msg = Value::int(1);
            for _ in 0..100 {
                rt.send_message(consumer, "handle", &[msg]);
            }
            for _ in 0..200 {
                rt.run_scheduler();
                rt.process_gc_ops();
            }
            black_box(consumer);
        })
    });
}

fn register_counter(rt: &mut Runtime, actor_id: u64) {
    rt.actors
        .get_mut(&actor_id)
        .expect("counter actor must exist")
        .register_behavior("inc", |actor, args| {
            let count = actor
                .get_state_field("count")
                .and_then(|v| v.as_int())
                .unwrap_or(0);
            let by = args.first().and_then(|v| v.as_int()).unwrap_or(1);
            actor.set_state_field("count", Value::int(count + by));
        });
}

/// `register_counter` registers exactly one behavior, so its id is 0.
/// Keep this explicit: name lookup cannot work from a non-owning shard.
fn shards_behavior_id_zero() -> u16 {
    0
}

fn counter_value(rt: &Runtime, actor_id: u64) -> i64 {
    rt.actors
        .get(&actor_id)
        .and_then(|actor| actor.get_state_field("count"))
        .and_then(|v| v.as_int())
        .unwrap_or(-1)
}

/// End-to-end same-shard cost for a meaningful burst: enqueue 10k value
/// messages, run to quiescence, and verify all state transitions happened.
///
/// Runtime construction is intentionally included. The paired cross-shard
/// benchmark below includes the same setup class, so their ratio remains useful
/// even though neither is a pure mailbox-only microbenchmark.
fn bench_same_shard_10k(c: &mut Criterion) {
    const MESSAGES: usize = 10_000;

    c.bench_function("actor/same_shard_10k", |b| {
        b.iter(|| {
            let mut rt = Runtime::new();
            let counter = rt.spawn_actor(Box::new(|| vec![("count".to_string(), Value::int(0))]));
            register_counter(&mut rt, counter);

            for _ in 0..MESSAGES {
                rt.send_message(counter, "inc", &[Value::int(1)]);
            }
            rt.run_scheduler();

            let count = counter_value(&rt, counter);
            debug_assert_eq!(count, MESSAGES as i64);
            black_box(count);
        })
    });
}

/// True cross-thread, cross-shard delivery. Shard 0 enqueues messages while
/// shard 1 runs its production scheduler on a separate OS thread. The bounded
/// 1024-entry cross-shard channel therefore exercises sender backpressure as
/// well as receiver drain and actor execution.
fn bench_cross_shard_threaded_10k(c: &mut Criterion) {
    const MESSAGES: usize = 10_000;

    c.bench_function("actor/cross_shard_threaded_10k", |b| {
        b.iter(|| {
            let mut shards = Runtime::new_sharded(2);

            // Actor ids are process-global. Spawn on shard 1 until the id also
            // maps to shard 1 so the routing invariant actor_id % shard_count
            // matches physical ownership.
            let mut counter =
                shards[1].spawn_actor(Box::new(|| vec![("count".to_string(), Value::int(0))]));
            while counter % 2 != 1 {
                counter =
                    shards[1].spawn_actor(Box::new(|| vec![("count".to_string(), Value::int(0))]));
            }
            register_counter(&mut shards[1], counter);

            let mut receiver = shards.pop().expect("shard 1");
            let mut sender = shards.pop().expect("shard 0");

            let receiver_thread = std::thread::spawn(move || loop {
                receiver.run_scheduler();
                let count = counter_value(&receiver, counter);
                if count >= MESSAGES as i64 {
                    return count;
                }
                std::thread::yield_now();
            });

            // `counter` is owned by shard 1, so shard 0 cannot resolve a
            // behavior name against its local actor table. Use the registered
            // behavior's stable numeric id directly instead of depending on
            // send_message's unknown-name fallback to behavior 0.
            let inc_behavior = shards_behavior_id_zero();
            for _ in 0..MESSAGES {
                sender.send_message_by_id(counter, inc_behavior, &[Value::int(1)]);
            }

            let count = receiver_thread.join().expect("receiver shard thread");
            debug_assert_eq!(count, MESSAGES as i64);
            black_box(count);
        })
    });
}

/// Actor creation scalability. Actors stay resident in the runtime for the
/// duration of the iteration so this measures allocation/registration cost,
/// not repeated create/drop of a single actor.
fn bench_spawn_10k(c: &mut Criterion) {
    const ACTORS: usize = 10_000;

    c.bench_function("actor/spawn_10k", |b| {
        b.iter(|| {
            let mut rt = Runtime::new();
            for _ in 0..ACTORS {
                black_box(rt.spawn_actor(Box::new(|| vec![])));
            }
            black_box(rt.actors.len());
        })
    });
}

criterion_group!(
    benches,
    bench_spawn_send_receive,
    bench_message_throughput,
    bench_same_shard_10k,
    bench_cross_shard_threaded_10k,
    bench_spawn_10k
);
