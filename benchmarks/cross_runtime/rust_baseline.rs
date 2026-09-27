use std::sync::mpsc;
use std::thread;
use std::time::Instant;

const COUNT_N: usize = 200_000;
const PING_N: usize = 20_000;
const RING: usize = 10;
const HOPS: i64 = 20_000;
const WORKERS: usize = 8;
const TASKS: usize = 50_000;

fn report(name: &str, messages: u64, elapsed: std::time::Duration) {
    println!(
        "[cross-bench] runtime=rust benchmark={} messages={} elapsed_ns={}",
        name,
        messages,
        elapsed.as_nanos()
    );
}

fn counting() {
    let (tx, rx) = mpsc::channel::<()>();
    let (done_tx, done_rx) = mpsc::channel::<usize>();
    let actor = thread::spawn(move || {
        let mut count = 0usize;
        while count < COUNT_N {
            rx.recv().expect("counting mailbox closed early");
            count += 1;
        }
        done_tx.send(count).unwrap();
    });

    let start = Instant::now();
    for _ in 0..COUNT_N {
        tx.send(()).unwrap();
    }
    let count = done_rx.recv().unwrap();
    let elapsed = start.elapsed();
    assert_eq!(count, COUNT_N);
    actor.join().unwrap();
    report("counting", COUNT_N as u64, elapsed);
}

enum PingMsg {
    Kick(usize),
    Ack(usize),
    Stop,
}

enum PongMsg {
    Recv,
    Stop,
}

fn ping_pong() {
    let (ping_tx, ping_rx) = mpsc::channel::<PingMsg>();
    let (pong_tx, pong_rx) = mpsc::channel::<PongMsg>();
    let (done_tx, done_rx) = mpsc::channel::<usize>();

    let ping_pong_tx = pong_tx.clone();
    let ping = thread::spawn(move || {
        let mut remaining = 0usize;
        loop {
            match ping_rx.recv().unwrap() {
                PingMsg::Kick(n) => {
                    remaining = n;
                    ping_pong_tx.send(PongMsg::Recv).unwrap();
                }
                PingMsg::Ack(pong_count) => {
                    remaining -= 1;
                    if remaining == 0 {
                        done_tx.send(pong_count).unwrap();
                    } else {
                        ping_pong_tx.send(PongMsg::Recv).unwrap();
                    }
                }
                PingMsg::Stop => break,
            }
        }
    });

    let pong_ping_tx = ping_tx.clone();
    let pong = thread::spawn(move || {
        let mut count = 0usize;
        loop {
            match pong_rx.recv().unwrap() {
                PongMsg::Recv => {
                    count += 1;
                    pong_ping_tx.send(PingMsg::Ack(count)).unwrap();
                }
                PongMsg::Stop => break,
            }
        }
    });

    let start = Instant::now();
    ping_tx.send(PingMsg::Kick(PING_N)).unwrap();
    let count = done_rx.recv().unwrap();
    let elapsed = start.elapsed();
    assert_eq!(count, PING_N);

    ping_tx.send(PingMsg::Stop).unwrap();
    pong_tx.send(PongMsg::Stop).unwrap();
    ping.join().unwrap();
    pong.join().unwrap();
    report("ping_pong", (2 * PING_N + 1) as u64, elapsed);
}

enum RingMsg {
    Token(i64, i64),
    Stop,
}

fn thread_ring() {
    let mut senders = Vec::with_capacity(RING);
    let mut receivers = Vec::with_capacity(RING);
    for _ in 0..RING {
        let (tx, rx) = mpsc::channel::<RingMsg>();
        senders.push(tx);
        receivers.push(Some(rx));
    }
    let (done_tx, done_rx) = mpsc::channel::<i64>();

    let mut actors = Vec::with_capacity(RING);
    for i in 0..RING {
        let rx = receivers[i].take().unwrap();
        let next = senders[(i + 1) % RING].clone();
        let done = done_tx.clone();
        actors.push(thread::spawn(move || loop {
            match rx.recv().unwrap() {
                RingMsg::Token(h, c) => {
                    if h > 0 {
                        next.send(RingMsg::Token(h - 1, c + 1)).unwrap();
                    } else {
                        done.send(c).unwrap();
                    }
                }
                RingMsg::Stop => break,
            }
        }));
    }

    let start = Instant::now();
    senders[0].send(RingMsg::Token(HOPS, 0)).unwrap();
    let total = done_rx.recv().unwrap();
    let elapsed = start.elapsed();
    assert_eq!(total, HOPS);

    for tx in &senders {
        tx.send(RingMsg::Stop).unwrap();
    }
    for actor in actors {
        actor.join().unwrap();
    }
    report("thread_ring", HOPS as u64, elapsed);
}

enum WorkerMsg {
    Task,
    Stop,
}

enum SinkMsg {
    Ack,
    Stop,
}

fn fork_join() {
    let (sink_tx, sink_rx) = mpsc::channel::<SinkMsg>();
    let (done_tx, done_rx) = mpsc::channel::<usize>();
    let sink = thread::spawn(move || {
        let mut count = 0usize;
        loop {
            match sink_rx.recv().unwrap() {
                SinkMsg::Ack => {
                    count += 1;
                    if count == TASKS {
                        done_tx.send(count).unwrap();
                    }
                }
                SinkMsg::Stop => break,
            }
        }
    });

    let mut worker_txs = Vec::with_capacity(WORKERS);
    let mut workers = Vec::with_capacity(WORKERS);
    for _ in 0..WORKERS {
        let (tx, rx) = mpsc::channel::<WorkerMsg>();
        let sink = sink_tx.clone();
        worker_txs.push(tx);
        workers.push(thread::spawn(move || loop {
            match rx.recv().unwrap() {
                WorkerMsg::Task => sink.send(SinkMsg::Ack).unwrap(),
                WorkerMsg::Stop => break,
            }
        }));
    }

    let start = Instant::now();
    for i in 0..TASKS {
        worker_txs[i % WORKERS].send(WorkerMsg::Task).unwrap();
    }
    let count = done_rx.recv().unwrap();
    let elapsed = start.elapsed();
    assert_eq!(count, TASKS);

    for tx in &worker_txs {
        tx.send(WorkerMsg::Stop).unwrap();
    }
    for worker in workers {
        worker.join().unwrap();
    }
    sink_tx.send(SinkMsg::Stop).unwrap();
    sink.join().unwrap();
    report("fork_join", (2 * TASKS) as u64, elapsed);
}

fn main() {
    counting();
    ping_pong();
    thread_ring();
    fork_join();
}
