//! MPSC mailbox admission under concurrent producers (enqueue-only lower bound).
//! The consumer deliberately does not run until all producers have completed.
//! Not a full actor-send benchmark: it excludes Runtime routing and scheduling.
//!
//! cargo run --release --no-default-features --example mailbox_contention -- 4 10000
use nulang::runtime::{Mailbox, Message, MessagePayload, MessagePriority};
use nulang::vm::Value;
use std::sync::{Arc, Barrier};
use std::time::{Duration, Instant};

#[derive(Debug)]
struct ResultRow {
    producers: usize,
    messages: usize,
    elapsed: Duration,
}

fn run(producers: usize, messages_per_producer: usize) -> ResultRow {
    assert!(producers > 0);
    assert!(messages_per_producer > 0);
    let expected = producers
        .checked_mul(messages_per_producer)
        .expect("message overflow");
    let mailbox = Arc::new(Mailbox::new(0));
    let barrier = Arc::new(Barrier::new(producers + 1));

    // Spawn threads before timing, so thread-creation latency is not attributed
    // to mailbox admission. Barrier wakeup and join are still included.
    let elapsed = std::thread::scope(|scope| {
        let mut handles = Vec::with_capacity(producers);
        for producer in 0..producers {
            let mailbox = Arc::clone(&mailbox);
            let barrier = Arc::clone(&barrier);
            handles.push(scope.spawn(move || {
                barrier.wait();
                for _ in 0..messages_per_producer {
                    mailbox
                        .push(Message {
                            behavior_id: 0,
                            payload: MessagePayload::from_slice(&[Value::int(1)]),
                            sender: producer as u64 + 1,
                            priority: MessagePriority::Normal,
                            trace_id: None,
                        })
                        .unwrap_or_else(|_| panic!("unbounded mailbox rejected message"));
                }
            }));
        }
        let start = Instant::now();
        barrier.wait();
        for handle in handles {
            handle.join().expect("mailbox producer panicked");
        }
        start.elapsed()
    });

    assert_eq!(
        mailbox.len(),
        expected,
        "concurrent producers lost messages"
    );
    let mut mailbox = Arc::try_unwrap(mailbox)
        .unwrap_or_else(|_| panic!("unexpected remaining mailbox references"));
    let mut drained = 0;
    while let Some(message) = mailbox.pop() {
        assert_eq!(message.payload.as_slice(), &[Value::int(1)]);
        drained += 1;
    }
    assert_eq!(drained, expected, "drain did not account for every message");
    ResultRow {
        producers,
        messages: expected,
        elapsed,
    }
}

fn main() {
    let args: Vec<String> = std::env::args().collect();
    if args.len() != 3 {
        eprintln!("usage: mailbox_contention <producer-count> <messages-per-producer>");
        std::process::exit(2);
    }
    let producers = args[1].parse::<usize>().expect("invalid producer count");
    let each = args[2]
        .parse::<usize>()
        .expect("invalid messages-per-producer");
    assert!((1..=64).contains(&producers), "producer count out of range");
    assert!((1..=100_000).contains(&each), "message batch out of range");
    let row = run(producers, each);
    println!(
        "[contention-bench] runtime=nulang producers={} messages={} elapsed_ns={}",
        row.producers,
        row.messages,
        row.elapsed.as_nanos()
    );
}

#[cfg(test)]
mod tests {
    use super::run;

    #[test]
    fn one_producer_does_not_lose_messages() {
        let row = run(1, 100);
        assert_eq!(row.messages, 100);
    }

    #[test]
    fn four_concurrent_producers_do_not_lose_messages() {
        let row = run(4, 100);
        assert_eq!(row.messages, 400);
    }
}
