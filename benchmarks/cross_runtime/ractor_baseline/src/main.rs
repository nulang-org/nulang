//! Stateless same-host actor-framework comparator.
//! Keep workload sizes and logical message definitions aligned with
//! src/bin/nulang_savina.rs. Only measured actor messaging is timed.

use ractor::{Actor, ActorProcessingErr, ActorRef};
use std::time::{Duration, Instant};
use tokio::sync::{mpsc, oneshot};

const COUNT_N: usize = 200_000;
const PING_N: usize = 20_000;
const RING: usize = 10;
const HOPS: i64 = 20_000;
const WORKERS: usize = 8;
const TASKS: usize = 50_000;

fn report(name: &str, messages: usize, elapsed: Duration) {
    assert!(elapsed.as_nanos() > 0, "timer must make progress");
    println!(
        "[cross-bench] runtime=ractor benchmark={name} messages={messages} elapsed_ns={}",
        elapsed.as_nanos()
    );
}

struct Counter;
enum CounterMsg {
    Inc,
}
struct CounterState {
    count: usize,
    done: Option<oneshot::Sender<usize>>,
}
impl Actor for Counter {
    type Msg = CounterMsg;
    type State = CounterState;
    type Arguments = oneshot::Sender<usize>;

    async fn pre_start(
        &self,
        _: ActorRef<Self::Msg>,
        done: Self::Arguments,
    ) -> Result<Self::State, ActorProcessingErr> {
        Ok(CounterState {
            count: 0,
            done: Some(done),
        })
    }

    async fn handle(
        &self,
        _: ActorRef<Self::Msg>,
        msg: Self::Msg,
        state: &mut Self::State,
    ) -> Result<(), ActorProcessingErr> {
        match msg {
            CounterMsg::Inc => {
                state.count += 1;
                if state.count == COUNT_N {
                    state.done.take().expect("counter completion already sent")
                        .send(state.count).expect("counter receiver disappeared");
                }
            }
        }
        Ok(())
    }
}

async fn counting() {
    let (done_tx, done_rx) = oneshot::channel();
    let (counter, task) = Counter::spawn(None, Counter, done_tx).await.unwrap();
    let start = Instant::now();
    for _ in 0..COUNT_N {
        counter.cast(CounterMsg::Inc).unwrap_or_else(|_| panic!("counter mailbox closed"));
    }
    assert_eq!(done_rx.await.unwrap(), COUNT_N);
    let elapsed = start.elapsed();
    counter.stop(None);
    task.await.unwrap();
    report("counting", COUNT_N, elapsed);
}

struct Ping;
enum PingMsg {
    Wire(ActorRef<PongMsg>, oneshot::Sender<()>),
    Kick(usize),
    Ack(usize),
}
struct PingState {
    pong: Option<ActorRef<PongMsg>>,
    remaining: usize,
    done: Option<oneshot::Sender<usize>>,
}
impl Actor for Ping {
    type Msg = PingMsg;
    type State = PingState;
    type Arguments = oneshot::Sender<usize>;

    async fn pre_start(
        &self,
        _: ActorRef<Self::Msg>,
        done: Self::Arguments,
    ) -> Result<Self::State, ActorProcessingErr> {
        Ok(PingState {
            pong: None,
            remaining: 0,
            done: Some(done),
        })
    }

    async fn handle(
        &self,
        _: ActorRef<Self::Msg>,
        msg: Self::Msg,
        state: &mut Self::State,
    ) -> Result<(), ActorProcessingErr> {
        match msg {
            PingMsg::Wire(pong, ready) => {
                state.pong = Some(pong);
                ready.send(()).expect("ping setup receiver disappeared");
            }
            PingMsg::Kick(rounds) => {
                state.remaining = rounds;
                state.pong.as_ref().expect("pong was not wired")
                    .cast(PongMsg::Recv).unwrap_or_else(|_| panic!("pong mailbox closed"));
            }
            PingMsg::Ack(count) => {
                state.remaining -= 1;
                if state.remaining == 0 {
                    state.done.take().expect("ping completion already sent")
                        .send(count).expect("ping receiver disappeared");
                } else {
                    state.pong.as_ref().expect("pong was not wired")
                        .cast(PongMsg::Recv).unwrap_or_else(|_| panic!("pong mailbox closed"));
                }
            }
        }
        Ok(())
    }
}

struct Pong;
enum PongMsg {
    Recv,
}
struct PongState {
    ping: ActorRef<PingMsg>,
    count: usize,
}
impl Actor for Pong {
    type Msg = PongMsg;
    type State = PongState;
    type Arguments = ActorRef<PingMsg>;

    async fn pre_start(
        &self,
        _: ActorRef<Self::Msg>,
        ping: Self::Arguments,
    ) -> Result<Self::State, ActorProcessingErr> {
        Ok(PongState { ping, count: 0 })
    }

    async fn handle(
        &self,
        _: ActorRef<Self::Msg>,
        msg: Self::Msg,
        state: &mut Self::State,
    ) -> Result<(), ActorProcessingErr> {
        match msg {
            PongMsg::Recv => {
                state.count += 1;
                state.ping.cast(PingMsg::Ack(state.count))
                    .unwrap_or_else(|_| panic!("ping mailbox closed"));
            }
        }
        Ok(())
    }
}

async fn ping_pong() {
    let (done_tx, done_rx) = oneshot::channel();
    let (ping, ping_task) = Ping::spawn(None, Ping, done_tx).await.unwrap();
    let (pong, pong_task) = Pong::spawn(None, Pong, ping.clone()).await.unwrap();
    let (ready_tx, ready_rx) = oneshot::channel();
    ping.cast(PingMsg::Wire(pong.clone(), ready_tx))
        .unwrap_or_else(|_| panic!("ping wiring failed"));
    ready_rx.await.unwrap();

    let start = Instant::now();
    ping.cast(PingMsg::Kick(PING_N))
        .unwrap_or_else(|_| panic!("ping kickoff failed"));
    assert_eq!(done_rx.await.unwrap(), PING_N);
    let elapsed = start.elapsed();
    ping.stop(None);
    pong.stop(None);
    ping_task.await.unwrap();
    pong_task.await.unwrap();
    report("ping_pong", 2 * PING_N + 1, elapsed);
}

struct RingActor;
enum RingMsg {
    Wire(ActorRef<RingMsg>, oneshot::Sender<()>),
    Token(i64, i64),
}
struct RingState {
    next: Option<ActorRef<RingMsg>>,
    done: mpsc::UnboundedSender<i64>,
}
impl Actor for RingActor {
    type Msg = RingMsg;
    type State = RingState;
    type Arguments = mpsc::UnboundedSender<i64>;

    async fn pre_start(
        &self,
        _: ActorRef<Self::Msg>,
        done: Self::Arguments,
    ) -> Result<Self::State, ActorProcessingErr> {
        Ok(RingState { next: None, done })
    }

    async fn handle(
        &self,
        _: ActorRef<Self::Msg>,
        msg: Self::Msg,
        state: &mut Self::State,
    ) -> Result<(), ActorProcessingErr> {
        match msg {
            RingMsg::Wire(next, ready) => {
                state.next = Some(next);
                ready.send(()).expect("ring setup receiver disappeared");
            }
            RingMsg::Token(h, count) => {
                if h > 0 {
                    state.next.as_ref().expect("ring link missing")
                        .cast(RingMsg::Token(h - 1, count + 1))
                        .unwrap_or_else(|_| panic!("next ring actor stopped"));
                } else {
                    state.done.send(count).expect("ring completion receiver disappeared");
                }
            }
        }
        Ok(())
    }
}

async fn thread_ring() {
    let (done_tx, mut done_rx) = mpsc::unbounded_channel();
    let mut actors = Vec::with_capacity(RING);
    let mut tasks = Vec::with_capacity(RING);
    for _ in 0..RING {
        let (actor, task) = RingActor::spawn(None, RingActor, done_tx.clone())
            .await.unwrap();
        actors.push(actor);
        tasks.push(task);
    }
    for i in 0..RING {
        let (ready_tx, ready_rx) = oneshot::channel();
        actors[i].cast(RingMsg::Wire(actors[(i + 1) % RING].clone(), ready_tx))
            .unwrap_or_else(|_| panic!("ring wiring failed"));
        ready_rx.await.unwrap();
    }

    let start = Instant::now();
    actors[0].cast(RingMsg::Token(HOPS, 0))
        .unwrap_or_else(|_| panic!("ring kickoff failed"));
    assert_eq!(done_rx.recv().await.unwrap(), HOPS);
    let elapsed = start.elapsed();
    for actor in &actors {
        actor.stop(None);
    }
    for task in tasks {
        task.await.unwrap();
    }
    report("thread_ring", HOPS as usize, elapsed);
}

struct Sink;
enum SinkMsg {
    Ack,
}
struct SinkState {
    count: usize,
    done: Option<oneshot::Sender<usize>>,
}
impl Actor for Sink {
    type Msg = SinkMsg;
    type State = SinkState;
    type Arguments = oneshot::Sender<usize>;

    async fn pre_start(
        &self,
        _: ActorRef<Self::Msg>,
        done: Self::Arguments,
    ) -> Result<Self::State, ActorProcessingErr> {
        Ok(SinkState { count: 0, done: Some(done) })
    }

    async fn handle(
        &self,
        _: ActorRef<Self::Msg>,
        msg: Self::Msg,
        state: &mut Self::State,
    ) -> Result<(), ActorProcessingErr> {
        match msg {
            SinkMsg::Ack => {
                state.count += 1;
                if state.count == TASKS {
                    state.done.take().expect("sink completion already sent")
                        .send(state.count).expect("sink receiver disappeared");
                }
            }
        }
        Ok(())
    }
}

struct Worker;
enum WorkerMsg {
    Task,
}
impl Actor for Worker {
    type Msg = WorkerMsg;
    type State = ActorRef<SinkMsg>;
    type Arguments = ActorRef<SinkMsg>;

    async fn pre_start(
        &self,
        _: ActorRef<Self::Msg>,
        sink: Self::Arguments,
    ) -> Result<Self::State, ActorProcessingErr> {
        Ok(sink)
    }

    async fn handle(
        &self,
        _: ActorRef<Self::Msg>,
        msg: Self::Msg,
        sink: &mut Self::State,
    ) -> Result<(), ActorProcessingErr> {
        match msg {
            WorkerMsg::Task => {
                sink.cast(SinkMsg::Ack).unwrap_or_else(|_| panic!("sink mailbox closed"));
            }
        }
        Ok(())
    }
}

async fn fork_join() {
    let (done_tx, done_rx) = oneshot::channel();
    let (sink, sink_task) = Sink::spawn(None, Sink, done_tx).await.unwrap();
    let mut workers = Vec::with_capacity(WORKERS);
    let mut tasks = Vec::with_capacity(WORKERS);
    for _ in 0..WORKERS {
        let (actor, task) = Worker::spawn(None, Worker, sink.clone()).await.unwrap();
        workers.push(actor);
        tasks.push(task);
    }

    let start = Instant::now();
    for i in 0..TASKS {
        workers[i % WORKERS].cast(WorkerMsg::Task)
            .unwrap_or_else(|_| panic!("worker mailbox closed"));
    }
    assert_eq!(done_rx.await.unwrap(), TASKS);
    let elapsed = start.elapsed();
    for actor in &workers {
        actor.stop(None);
    }
    sink.stop(None);
    for task in tasks {
        task.await.unwrap();
    }
    sink_task.await.unwrap();
    report("fork_join", 2 * TASKS, elapsed);
}

// A single Tokio execution worker matches Nulang's existing single-shard
// Savina harness. CPU affinity is additionally enforced by the Python runner.
#[tokio::main(flavor = "current_thread")]
async fn main() {
    counting().await;
    ping_pong().await;
    thread_ring().await;
    fork_join().await;
}
