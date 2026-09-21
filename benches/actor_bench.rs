//! Actor throughput benchmarks.

use criterion::{black_box, criterion_group, BatchSize, BenchmarkId, Criterion};
use nulang::runtime::{Mailbox, Message, MessagePriority, Runtime};
use nulang::vm::Value;
use std::sync::Arc;

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

fn selective_receive_mailbox(depth: usize, hit_behavior: u16) -> Mailbox {
    let mut mailbox = Mailbox::new(0);
    for _ in 0..depth.saturating_sub(1) {
        mailbox
            .push_local(Message {
                behavior_id: 1,
                payload: Arc::new(vec![Value::int(1)]),
                sender: 0,
                priority: MessagePriority::Normal,
                trace_id: None,
            })
            .unwrap();
    }
    mailbox
        .push_local(Message {
            behavior_id: hit_behavior,
            payload: Arc::new(vec![Value::int(42)]),
            sender: 0,
            priority: MessagePriority::Normal,
            trace_id: None,
        })
        .unwrap();
    mailbox
}

fn bench_selective_receive(c: &mut Criterion) {
    const HIT: u16 = 60_000;
    let mut group = c.benchmark_group("actor/selective_receive_depth");
    for depth in [64usize, 1024, 16_384] {
        group.bench_with_input(BenchmarkId::from_parameter(depth), &depth, |b, &depth| {
            b.iter_batched(
                || selective_receive_mailbox(depth, HIT),
                |mut mailbox| black_box(mailbox.receive_match(black_box(&[HIT]))),
                BatchSize::SmallInput,
            )
        });
    }
    group.finish();

    let mut group = c.benchmark_group("actor/selective_receive_arms");
    for arm_count in [1usize, 8, 32] {
        let mut behavior_ids: Vec<u16> = (10_000..10_000 + arm_count as u16).collect();
        behavior_ids.push(HIT);
        group.bench_with_input(
            BenchmarkId::from_parameter(arm_count),
            &arm_count,
            |b, _| {
                b.iter_batched(
                    || selective_receive_mailbox(4096, HIT),
                    |mut mailbox| {
                        black_box(mailbox.receive_match(black_box(behavior_ids.as_slice())))
                    },
                    BatchSize::SmallInput,
                )
            },
        );
    }
    group.finish();
}

criterion_group!(
    benches,
    bench_spawn_send_receive,
    bench_message_throughput,
    bench_selective_receive
);
