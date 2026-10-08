//! Ractor many-to-one actor-mailbox admission, separate from handler execution.
//! Uses a current-thread Tokio executor: producer std::threads enqueue messages
//! while the actor's execution thread is blocked waiting for producer completion.
//! Includes ActorRef::cast routing, unlike Nulang's raw Mailbox::push lower bound.
use ractor::{Actor, ActorProcessingErr, ActorRef};
use std::sync::{Arc, Barrier};
use std::time::Instant;
use tokio::sync::oneshot;

struct Receiver;
enum Incoming { One }
struct ReceiverState {
    observed: usize,
    expected: usize,
    done: Option<oneshot::Sender<usize>>,
}
impl Actor for Receiver {
    type Msg = Incoming;
    type State = ReceiverState;
    type Arguments = (usize, oneshot::Sender<usize>);

    async fn pre_start(
        &self,
        _: ActorRef<Self::Msg>,
        (expected, done): Self::Arguments,
    ) -> Result<Self::State, ActorProcessingErr> {
        Ok(ReceiverState { observed: 0, expected, done: Some(done) })
    }

    async fn handle(
        &self,
        _: ActorRef<Self::Msg>,
        msg: Self::Msg,
        state: &mut Self::State,
    ) -> Result<(), ActorProcessingErr> {
        match msg {
            Incoming::One => {
                state.observed += 1;
                if state.observed == state.expected {
                    state.done.take().expect("duplicate completion")
                        .send(state.observed).expect("completion receiver closed");
                }
            }
        }
        Ok(())
    }
}

#[tokio::main(flavor = "current_thread")]
async fn main() {
    let args: Vec<_> = std::env::args().collect();
    if args.len() != 3 {
        eprintln!("usage: contention <producer-count> <messages-per-producer>");
        std::process::exit(2);
    }
    let producers: usize = args[1].parse().expect("invalid producer count");
    let each: usize = args[2].parse().expect("invalid messages-per-producer");
    assert!((1..=64).contains(&producers), "producer count out of range");
    assert!((1..=100_000).contains(&each), "message batch out of range");
    let expected = producers.checked_mul(each).expect("message count overflow");

    let (done_tx, done_rx) = oneshot::channel();
    let (receiver, task) = Receiver::spawn(None, Receiver, (expected, done_tx))
        .await.expect("receiver actor startup failed");
    let barrier = Arc::new(Barrier::new(producers + 1));

    // Producer allocation and thread spawn are excluded from measured work.
    // Scheduler/actor handler runs only after the blocking scope returns.
    let elapsed = std::thread::scope(|scope| {
        let mut tasks = Vec::with_capacity(producers);
        for _ in 0..producers {
            let actor = receiver.clone();
            let barrier = Arc::clone(&barrier);
            tasks.push(scope.spawn(move || {
                barrier.wait();
                for _ in 0..each {
                    actor.cast(Incoming::One)
                        .unwrap_or_else(|_| panic!("receiver rejected enqueue"));
                }
            }));
        }
        let start = Instant::now();
        barrier.wait();
        for task in tasks {
            task.join().expect("producer thread panicked");
        }
        start.elapsed()
    });

    // Validate every message after timing; dropping the future before
    // completion must be treated as benchmark failure, not a fast result.
    assert_eq!(done_rx.await.expect("receiver completion disappeared"), expected);
    receiver.stop(None);
    task.await.expect("actor shutdown panicked");
    println!(
        "[contention-bench] runtime=ractor producers={} messages={} elapsed_ns={}",
        producers, expected, elapsed.as_nanos()
    );
}
