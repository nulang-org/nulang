//! Exact-base cross-shard receive-pass diagnostic.
//!
//! The Python A/B harness overlays this identical file onto its detached
//! base worktree before compiling both variants. Enqueue and actor creation
//! occur outside the timed region; each timed segment measures one scheduler
//! pass that drains and executes a fixed-size batch on a target shard.
//!
//! Usage: cargo run --release --no-default-features --example
//!   cross_shard_drain -- 4 2000

use std::time::Instant;

use nulang::runtime::{Actor, Runtime};
use nulang::vm::Value;

fn count(actor: &mut Actor, _args: &[Value]) {
    let old = actor
        .get_state_field("count")
        .and_then(|value| value.as_int())
        .unwrap_or(0);
    actor.set_state_field("count", Value::int(old + 1));
}

fn main() {
    let mut args = std::env::args().skip(1);
    let batch: usize = args
        .next()
        .expect("batch size argument is required")
        .parse()
        .expect("batch must be an integer");
    let rounds: usize = args
        .next()
        .expect("rounds argument is required")
        .parse()
        .expect("rounds must be an integer");
    assert!(
        (1..=64).contains(&batch) && rounds > 0,
        "batch must be 1..=64, rounds must be positive"
    );
    assert!(args.next().is_none(), "unexpected extra arguments");

    let mut shards = Runtime::new_sharded(2);
    let mut target = shards[1].spawn_actor(Box::new(|| {
        vec![("count".to_string(), Value::int(0))]
    }));
    while target % 2 != 1 {
        target = shards[1].spawn_actor(Box::new(|| {
            vec![("count".to_string(), Value::int(0))]
        }));
    }
    shards[1]
        .actors
        .get_mut(&target)
        .expect("actor exists")
        .register_behavior("count", count);
    shards[1].run_scheduler(); // drain spawn-time scheduler activity

    let mut elapsed_ns = 0u128;
    for _ in 0..rounds {
        for _ in 0..batch {
            shards[0].send_message_by_id(target, 0, &[]);
        }
        let start = Instant::now();
        shards[1].run_scheduler();
        elapsed_ns += start.elapsed().as_nanos();
    }

    let actual = shards[1]
        .actors
        .get(&target)
        .and_then(|actor| actor.get_state_field("count"))
        .and_then(|value| value.as_int());
    assert_eq!(
        actual,
        Some((rounds * batch) as i64),
        "every enqueued cross-shard message must execute exactly once"
    );
    println!(
        "[cross-shard-drain] batch={batch} rounds={rounds} elapsed_ns={elapsed_ns}"
    );
}
