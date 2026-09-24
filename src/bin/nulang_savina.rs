//! Standalone Savina-style actor benchmark runner.
//!
//! This binary intentionally depends only on Nulang's core compiler/runtime path.
//! Run it with:
//!
//! cargo run --locked --profile savina --no-default-features --features savina-bench \
//!   --bin nulang-savina -- --format jsonl

use std::cell::RefCell;
use std::env;
use std::process::ExitCode;
use std::rc::Rc;
use std::sync::{
    atomic::{AtomicBool, Ordering},
    Arc, Barrier,
};
use std::time::{Duration, Instant};

use nulang::lexer::Lexer;
use nulang::parser::Parser;
use nulang::runtime::{Runtime, RuntimeVmCallbacks};
use nulang::typechecker::TypeChecker;
use nulang::vm::{Value, VM};
use serde_json::json;

const BENCHMARKS: &[&str] = &[
    "counting",
    "ping_pong",
    "thread_ring",
    "fork_join",
    "skynet",
];

#[derive(Clone, Copy)]
enum OutputFormat {
    Human,
    Jsonl,
}

struct Config {
    format: OutputFormat,
    benchmark: Option<String>,
    repeat: u32,
    /// Number of real Runtime shards used by workloads that provide an
    /// explicit sharded fixture. Today that is fork_join only.
    shards: usize,
}

#[derive(Debug)]
struct Measurement {
    benchmark: &'static str,
    messages: u64,
    elapsed: Duration,
    shards: usize,
}

impl Measurement {
    fn messages_per_second(&self) -> f64 {
        self.messages as f64 / self.elapsed.as_secs_f64()
    }

    fn ns_per_message(&self) -> f64 {
        self.elapsed.as_nanos() as f64 / self.messages as f64
    }
}

fn main() -> ExitCode {
    let config = match parse_args() {
        Ok(Some(config)) => config,
        Ok(None) => return ExitCode::SUCCESS,
        Err(message) => {
            eprintln!("{message}");
            print_usage();
            return ExitCode::from(2);
        }
    };

    if let Some(name) = config.benchmark.as_deref() {
        if !BENCHMARKS.contains(&name) {
            eprintln!("unknown benchmark: {name}");
            eprintln!("available: {}", BENCHMARKS.join(", "));
            return ExitCode::from(2);
        }
    }

    for iteration in 1..=config.repeat {
        for &name in BENCHMARKS {
            if config
                .benchmark
                .as_deref()
                .is_some_and(|selected| selected != name)
            {
                continue;
            }

            let measurement = match name {
                "counting" => bench_counting(),
                "ping_pong" => bench_ping_pong(),
                "thread_ring" => bench_thread_ring(),
                "fork_join" if config.shards > 1 => bench_fork_join_sharded(config.shards),
                "fork_join" => bench_fork_join(),
                "skynet" => bench_skynet(),
                _ => unreachable!("benchmark list and dispatcher must stay in sync"),
            };
            emit(&measurement, iteration, config.format);
        }
    }

    ExitCode::SUCCESS
}

fn parse_args() -> Result<Option<Config>, String> {
    let mut format = OutputFormat::Human;
    let mut benchmark = None;
    let mut repeat = 1u32;
    let mut shards = 1usize;
    let mut args = env::args().skip(1);

    while let Some(arg) = args.next() {
        match arg.as_str() {
            "--format" => {
                let value = args
                    .next()
                    .ok_or_else(|| "--format requires human or jsonl".to_string())?;
                format = match value.as_str() {
                    "human" => OutputFormat::Human,
                    "jsonl" => OutputFormat::Jsonl,
                    _ => return Err(format!("unsupported format: {value}")),
                };
            }
            "--benchmark" => {
                benchmark = Some(
                    args.next()
                        .ok_or_else(|| "--benchmark requires a name".to_string())?,
                );
            }
            "--repeat" => {
                let value = args
                    .next()
                    .ok_or_else(|| "--repeat requires a positive integer".to_string())?;
                repeat = value
                    .parse::<u32>()
                    .map_err(|_| format!("invalid repeat count: {value}"))?;
                if repeat == 0 {
                    return Err("--repeat must be at least 1".to_string());
                }
            }
            "--shards" => {
                let value = args
                    .next()
                    .ok_or_else(|| "--shards requires a positive integer".to_string())?;
                shards = value
                    .parse::<usize>()
                    .map_err(|_| format!("invalid shard count: {value}"))?;
                if shards == 0 {
                    return Err("--shards must be at least 1".to_string());
                }
            }
            "--list" => {
                for name in BENCHMARKS {
                    println!("{name}");
                }
                return Ok(None);
            }
            "-h" | "--help" => {
                print_usage();
                return Ok(None);
            }
            _ => return Err(format!("unknown argument: {arg}")),
        }
    }

    Ok(Some(Config {
        format,
        benchmark,
        repeat,
        shards,
    }))
}

fn print_usage() {
    eprintln!(
        "Usage: nulang-savina [--format human|jsonl] [--benchmark NAME] [--repeat N] [--shards N] [--list]"
    );
}

fn emit(measurement: &Measurement, iteration: u32, format: OutputFormat) {
    match format {
        OutputFormat::Human => {
            println!(
                "[bench] {}: {} msgs in {:.3}s = {:.0} msg/s ({:.1} ns/msg), shards={}",
                measurement.benchmark,
                measurement.messages,
                measurement.elapsed.as_secs_f64(),
                measurement.messages_per_second(),
                measurement.ns_per_message(),
                measurement.shards
            );
            println!(
                "[cross-bench] runtime=nulang benchmark={} messages={} elapsed_ns={} shards={}",
                measurement.benchmark,
                measurement.messages,
                measurement.elapsed.as_nanos(),
                measurement.shards
            );
        }
        OutputFormat::Jsonl => {
            let elapsed_ns = u64::try_from(measurement.elapsed.as_nanos())
                .expect("benchmark duration must fit in u64 nanoseconds");
            println!(
                "{}",
                json!({
                    "schema": 2,
                    "runtime": "nulang",
                    "suite": "savina-style",
                    "benchmark": measurement.benchmark,
                    "iteration": iteration,
                    "messages": measurement.messages,
                    "elapsed_ns": elapsed_ns,
                    "messages_per_second": measurement.messages_per_second(),
                    "ns_per_message": measurement.ns_per_message(),
                    "shards": measurement.shards,
                })
            );
        }
    }
}

fn compile_module(source: &str) -> nulang::bytecode::CodeModule {
    let mut lexer = Lexer::new(source);
    let tokens = lexer.lex().expect("bench: lex failed");
    let mut parser = Parser::new(tokens);
    let ast = parser.parse_module().expect("bench: parse failed");
    let mut type_checker = TypeChecker::new();
    type_checker
        .check_module(&ast)
        .expect("bench: typecheck failed");
    let hir = nulang::hir_lower::lower_module(&ast, &type_checker.inferred_decl_types);
    let mut mir = nulang::mir_lower::lower_module(&hir).expect("bench: MIR lower failed");
    nulang::mir_codegen::compile_mir(&mut mir, "savina").expect("bench: codegen failed")
}

fn compile_run_with_runtime(source: &str, runtime: Rc<RefCell<Runtime>>) -> Value {
    let module = compile_module(source);
    let mut vm = VM::new();
    vm.load_module(module);
    vm.set_actor_callbacks(Box::new(RuntimeVmCallbacks::new(runtime)));
    vm.run().expect("bench: VM run failed")
}

fn bench_counting() -> Measurement {
    const N: i64 = 200_000;
    let source = r#"
        actor Counter {
            state count = 0
            behavior inc() { self.count = self.count + 1 }
        }
        spawn Counter {}
    "#;
    let rt = Rc::new(RefCell::new(Runtime::new()));
    let actor_id = compile_run_with_runtime(source, rt.clone())
        .as_actor_id()
        .expect("spawn returns an actor ref");

    let start = Instant::now();
    for _ in 0..N {
        rt.borrow_mut().send_message(actor_id, "inc", &[]);
    }
    rt.borrow_mut().run_scheduler();
    let elapsed = start.elapsed();

    let count = rt
        .borrow()
        .actors
        .get(&actor_id)
        .and_then(|a| a.get_state_field("count"))
        .and_then(|v| v.as_int());
    assert_eq!(count, Some(N), "counting actor must process every message");

    Measurement {
        benchmark: "counting",
        messages: N as u64,
        elapsed,
        shards: 1,
    }
}

fn bench_ping_pong() -> Measurement {
    const N: i64 = 20_000;
    let source = r#"
        actor Ping {
            state ponger = nil
            state remaining = 0
            behavior wire_ping(p) { self.ponger = p }
            behavior kick(n) {
                self.remaining = n
                send self.ponger recv()
            }
            behavior ack() {
                self.remaining = self.remaining - 1
                if self.remaining > 0 then send self.ponger recv() else unit
            }
        }
        actor Pong {
            state pinger = nil
            state count = 0
            behavior wire_pong(p) { self.pinger = p }
            behavior recv() {
                self.count = self.count + 1
                send self.pinger ack()
            }
        }
        let pinger = spawn Ping {} in
        let ponger = spawn Pong {} in {
            send pinger wire_ping(ponger)
            send ponger wire_pong(pinger)
            pinger
        }
    "#;
    let rt = Rc::new(RefCell::new(Runtime::new()));
    let pinger = compile_run_with_runtime(source, rt.clone())
        .as_actor_id()
        .expect("spawn returns an actor ref");
    let ponger = {
        let rt = rt.borrow();
        rt.actors
            .keys()
            .copied()
            .find(|&id| id != pinger)
            .expect("ponger spawned")
    };

    rt.borrow_mut().run_scheduler();

    let start = Instant::now();
    rt.borrow_mut()
        .send_message(pinger, "kick", &[Value::int(N)]);
    rt.borrow_mut().run_scheduler();
    let elapsed = start.elapsed();

    let count = rt
        .borrow()
        .actors
        .get(&ponger)
        .and_then(|a| a.get_state_field("count"))
        .and_then(|v| v.as_int());
    assert_eq!(count, Some(N), "ponger must receive exactly N pings");

    Measurement {
        benchmark: "ping_pong",
        messages: 2 * N as u64 + 1,
        elapsed,
        shards: 1,
    }
}

fn bench_thread_ring() -> Measurement {
    const RING: usize = 10;
    const HOPS: i64 = 20_000;

    let mut source = String::from(
        r#"actor Ring {
    state next = nil
    state sink = nil
    behavior setup(n, s) {
        self.next = n
        self.sink = s
    }
    behavior token(h, c) {
        if h > 0 then send self.next token(h - 1, c + 1) else send self.sink done(c)
    }
}
actor Sink {
    state total = -1
    behavior done(n) { self.total = n }
}
let s = spawn Sink {} in
"#,
    );
    for i in 0..RING {
        source.push_str(&format!("let r{i} = spawn Ring {{}} in\n"));
    }
    source.push_str("{\n");
    for i in 0..RING {
        let next = (i + 1) % RING;
        source.push_str(&format!("send r{i} setup(r{next}, s)\n"));
    }
    source.push_str("r0\n}\n");

    let rt = Rc::new(RefCell::new(Runtime::new()));
    let r0 = compile_run_with_runtime(&source, rt.clone())
        .as_actor_id()
        .expect("spawn returns an actor ref");
    let sink = {
        let rt = rt.borrow();
        *rt.actors.keys().min().expect("sink spawned first")
    };

    rt.borrow_mut().run_scheduler();

    let start = Instant::now();
    rt.borrow_mut()
        .send_message(r0, "token", &[Value::int(HOPS), Value::int(0)]);
    rt.borrow_mut().run_scheduler();
    let elapsed = start.elapsed();

    let total = rt
        .borrow()
        .actors
        .get(&sink)
        .and_then(|a| a.get_state_field("total"))
        .and_then(|v| v.as_int());
    assert_eq!(total, Some(HOPS), "token must complete exactly H hops");

    Measurement {
        benchmark: "thread_ring",
        messages: HOPS as u64,
        elapsed,
        shards: 1,
    }
}

fn bench_fork_join() -> Measurement {
    const WORKERS: usize = 8;
    const TASKS: i64 = 50_000;

    let mut source = String::from(
        r#"actor Worker {
    state count = 0
    state sink = nil
    behavior wire_worker(s) { self.sink = s }
    behavior task(n) {
        self.count = self.count + 1
        send self.sink ack()
    }
}
actor Sink {
    state count = 0
    behavior ack() { self.count = self.count + 1 }
}
let s = spawn Sink {} in
"#,
    );
    for i in 0..WORKERS {
        source.push_str(&format!("let w{i} = spawn Worker {{}} in\n"));
    }
    source.push_str("{\n");
    for i in 0..WORKERS {
        source.push_str(&format!("send w{i} wire_worker(s)\n"));
    }
    source.push_str("s\n}\n");

    let rt = Rc::new(RefCell::new(Runtime::new()));
    let sink = compile_run_with_runtime(&source, rt.clone())
        .as_actor_id()
        .expect("spawn returns an actor ref");
    let worker_ids: Vec<u64> = rt
        .borrow()
        .actors
        .keys()
        .copied()
        .filter(|&id| id != sink)
        .collect();
    assert_eq!(worker_ids.len(), WORKERS, "exactly W workers spawned");

    rt.borrow_mut().run_scheduler();

    let start = Instant::now();
    for i in 0..TASKS {
        let worker = worker_ids[(i as usize) % WORKERS];
        rt.borrow_mut()
            .send_message(worker, "task", &[Value::int(i)]);
    }
    rt.borrow_mut().run_scheduler();
    let elapsed = start.elapsed();

    let count = rt
        .borrow()
        .actors
        .get(&sink)
        .and_then(|a| a.get_state_field("count"))
        .and_then(|v| v.as_int());
    assert_eq!(count, Some(TASKS), "sink must ack every task");

    Measurement {
        benchmark: "fork_join",
        messages: 2 * TASKS as u64,
        elapsed,
        shards: 1,
    }
}

fn bytecode_behavior_id(module: &nulang::bytecode::CodeModule, qualified: &str) -> u16 {
    module
        .behaviors
        .iter()
        .position(|behavior| behavior.name == qualified)
        .unwrap_or_else(|| panic!("benchmark behavior {qualified} must exist")) as u16
}

/// Create one plain bytecode actor directly on its owning shard.
///
/// Runtime::spawn_from_module draws a fresh id without regard to the Runtime
/// instance on which it is called. For a benchmark that owns several Runtime
/// shards explicitly, use spawn_actor_near as an id-placement primitive and
/// then attach the same plain-actor bytecode metadata that spawn_from_module
/// would install. Setup is outside the timed region.
fn attach_plain_bytecode_actor(
    runtime: &mut Runtime,
    shard_hint: usize,
    module: &nulang::bytecode::CodeModule,
    init: Vec<(String, Value)>,
) -> u64 {
    let actor_id = runtime.spawn_actor_near(shard_hint as u64, Box::new(move || init));
    assert_eq!(
        actor_id % runtime.shard_count as u64,
        runtime.shard_idx as u64,
        "benchmark actor must be physically created on its owning shard"
    );

    let offsets: Vec<usize> = module
        .behaviors
        .iter()
        .map(|behavior| behavior.code_offset)
        .collect();
    let compensation_offsets: Vec<Option<usize>> = module
        .behaviors
        .iter()
        .map(|behavior| behavior.compensate_offset)
        .collect();

    let actor = runtime
        .actors
        .get_mut(&actor_id)
        .expect("fresh benchmark actor");
    actor.bytecode_module = Some(module.clone());
    actor.bytecode_offsets = offsets;
    actor.compensation_offsets = compensation_offsets;
    actor_id
}

/// True multi-shard fork/join fixture.
///
/// Eight worker actors are distributed round-robin across real Runtime shards.
/// A producer actor on shard 0 maintains a 512-task in-flight window: this is
/// below the 1024-entry cross-shard bus capacity, so the fixture measures
/// scheduler/transport/handler throughput rather than silently dropping a
/// preloaded 50k-message burst on bounded channels.
///
/// All actors, bytecode attachment, scheduler warmup and OS-thread creation are
/// outside the timed region. A barrier releases all shard scheduler threads at
/// once; completion is the producer observing every worker acknowledgement.
fn bench_fork_join_sharded(shard_count: usize) -> Measurement {
    const WORKERS: usize = 8;
    const TASKS: i64 = 50_000;
    const WINDOW: i64 = 512;

    assert!(shard_count > 1, "sharded fork_join requires at least two shards");
    assert!(
        shard_count <= WORKERS,
        "sharded fork_join supports at most one shard per worker ({WORKERS})"
    );

    let source = format!(
        r#"
actor Producer {{
    state w0 = nil
    state w1 = nil
    state w2 = nil
    state w3 = nil
    state w4 = nil
    state w5 = nil
    state w6 = nil
    state w7 = nil
    state total = 0
    state sent = 0
    state completed = 0
    state next = 0

    behavior warm() {{ unit }}

    behavior kick(n) {{
        self.total = n
        self.sent = 0
        self.completed = 0
        self.next = 0
        while self.sent < {WINDOW} {{
            if self.next == 0 then send self.w0 task(self.sent) else unit
            if self.next == 1 then send self.w1 task(self.sent) else unit
            if self.next == 2 then send self.w2 task(self.sent) else unit
            if self.next == 3 then send self.w3 task(self.sent) else unit
            if self.next == 4 then send self.w4 task(self.sent) else unit
            if self.next == 5 then send self.w5 task(self.sent) else unit
            if self.next == 6 then send self.w6 task(self.sent) else unit
            if self.next == 7 then send self.w7 task(self.sent) else unit
            if self.next == 7 then self.next = 0 else self.next = self.next + 1
            self.sent = self.sent + 1
        }}
    }}

    behavior ack() {{
        self.completed = self.completed + 1
        if self.sent < self.total then {{
            if self.next == 0 then send self.w0 task(self.sent) else unit
            if self.next == 1 then send self.w1 task(self.sent) else unit
            if self.next == 2 then send self.w2 task(self.sent) else unit
            if self.next == 3 then send self.w3 task(self.sent) else unit
            if self.next == 4 then send self.w4 task(self.sent) else unit
            if self.next == 5 then send self.w5 task(self.sent) else unit
            if self.next == 6 then send self.w6 task(self.sent) else unit
            if self.next == 7 then send self.w7 task(self.sent) else unit
            if self.next == 7 then self.next = 0 else self.next = self.next + 1
            self.sent = self.sent + 1
        }} else unit
    }}
}}

actor Worker {{
    state producer = nil
    state count = 0
    behavior warm() {{ unit }}
    behavior task(n) {{
        self.count = self.count + 1
        send self.producer ack()
    }}
}}

unit
"#
    );
    let module = compile_module(&source);
    let producer_warm = bytecode_behavior_id(&module, "Producer.warm");
    let producer_kick = bytecode_behavior_id(&module, "Producer.kick");
    let worker_warm = bytecode_behavior_id(&module, "Worker.warm");

    let mut shards = Runtime::new_sharded(shard_count);
    let producer = attach_plain_bytecode_actor(
        &mut shards[0],
        0,
        &module,
        vec![
            ("w0".to_string(), Value::nil()),
            ("w1".to_string(), Value::nil()),
            ("w2".to_string(), Value::nil()),
            ("w3".to_string(), Value::nil()),
            ("w4".to_string(), Value::nil()),
            ("w5".to_string(), Value::nil()),
            ("w6".to_string(), Value::nil()),
            ("w7".to_string(), Value::nil()),
            ("total".to_string(), Value::int(0)),
            ("sent".to_string(), Value::int(0)),
            ("completed".to_string(), Value::int(0)),
            ("next".to_string(), Value::int(0)),
        ],
    );

    let mut workers = Vec::with_capacity(WORKERS);
    for index in 0..WORKERS {
        let owner = index % shard_count;
        let worker = attach_plain_bytecode_actor(
            &mut shards[owner],
            owner,
            &module,
            vec![
                ("producer".to_string(), Value::actor_ref(producer)),
                ("count".to_string(), Value::int(0)),
            ],
        );
        workers.push((owner, worker));
    }

    {
        let producer_actor = shards[0]
            .actors
            .get_mut(&producer)
            .expect("producer actor");
        for (index, &(_, worker_id)) in workers.iter().enumerate() {
            producer_actor.set_state_field(
                format!("w{index}"),
                Value::actor_ref(worker_id),
            );
        }
    }

    // Warm every actor through a no-op bytecode behavior. This materializes
    // each actor's runtime-VM module index outside the measured region, matching
    // the benchmark rule that compilation/module wiring is setup rather than
    // message throughput.
    shards[0].send_message_by_id(producer, producer_warm, &[]);
    for &(owner, worker_id) in &workers {
        shards[owner].send_message_by_id(worker_id, worker_warm, &[]);
    }
    for shard in &mut shards {
        shard.run_scheduler();
    }

    // Admit the one control message before the barrier. It remains queued on
    // shard 0 and is part of neither the reported 100k logical message count
    // nor OS-thread startup.
    shards[0].send_message_by_id(producer, producer_kick, &[Value::int(TASKS)]);

    let barrier = Arc::new(Barrier::new(shard_count + 1));
    let done = Arc::new(AtomicBool::new(false));

    let (finished_shards, elapsed) = std::thread::scope(|scope| {
        let mut handles = Vec::with_capacity(shard_count);
        for (index, mut runtime) in shards.into_iter().enumerate() {
            let barrier = Arc::clone(&barrier);
            let done = Arc::clone(&done);
            handles.push(scope.spawn(move || {
                barrier.wait();
                loop {
                    runtime.run_scheduler();

                    if index == 0 {
                        let completed = runtime
                            .actors
                            .get(&producer)
                            .and_then(|actor| actor.get_state_field("completed"))
                            .and_then(|value| value.as_int())
                            .unwrap_or(0);
                        if completed >= TASKS {
                            done.store(true, Ordering::Release);
                        }
                    }
                    if done.load(Ordering::Acquire) {
                        break;
                    }
                    std::thread::yield_now();
                }
                runtime
            }));
        }

        // Threads exist and are parked at the barrier before the clock starts.
        let start = Instant::now();
        barrier.wait();
        let runtimes: Vec<Runtime> = handles
            .into_iter()
            .map(|handle| handle.join().expect("Savina shard thread panicked"))
            .collect();
        (runtimes, start.elapsed())
    });

    let completed = finished_shards[0]
        .actors
        .get(&producer)
        .and_then(|actor| actor.get_state_field("completed"))
        .and_then(|value| value.as_int());
    assert_eq!(
        completed,
        Some(TASKS),
        "producer must observe every worker acknowledgement"
    );

    let worker_total: i64 = workers
        .iter()
        .map(|&(owner, actor_id)| {
            finished_shards[owner]
                .actors
                .get(&actor_id)
                .and_then(|actor| actor.get_state_field("count"))
                .and_then(|value| value.as_int())
                .unwrap_or(-1)
        })
        .sum();
    assert_eq!(
        worker_total, TASKS,
        "workers must process exactly the requested task count"
    );

    Measurement {
        benchmark: "fork_join",
        messages: 2 * TASKS as u64,
        elapsed,
        shards: shard_count,
    }
}

fn bench_skynet() -> Measurement {
    const DEPTH: i64 = 3;
    const EXPECTED: i64 = 1111;

    let mut source = String::from(
        r#"actor Skynet {
    state parent = nil
    state remaining = 0
    state acc = 0
    state total = 0
    behavior begin(p, lvl) {
        self.parent = p
        self.remaining = 10
        self.acc = 1
        if lvl > 0 then {
"#,
    );
    for i in 0..10 {
        source.push_str(&format!(
            "let c{i} = spawn Skynet {{}} in send c{i} begin(self, lvl - 1)\n"
        ));
    }
    source.push_str(
        r#"        } else send self.parent result(1)
    }
    behavior result(v) {
        self.acc = self.acc + v
        self.remaining = self.remaining - 1
        if self.remaining == 0 then {
            if self.parent == nil then { self.total = self.acc } else send self.parent result(self.acc)
        }
    }
}
spawn Skynet {}
"#,
    );

    let rt = Rc::new(RefCell::new(Runtime::new()));
    let root = compile_run_with_runtime(&source, rt.clone())
        .as_actor_id()
        .expect("spawn returns an actor ref");

    let start = Instant::now();
    rt.borrow_mut()
        .send_message(root, "begin", &[Value::nil(), Value::int(DEPTH)]);
    rt.borrow_mut().run_scheduler();
    let elapsed = start.elapsed();

    let total = rt
        .borrow()
        .actors
        .get(&root)
        .and_then(|a| a.get_state_field("total"))
        .and_then(|v| v.as_int());
    assert_eq!(total, Some(EXPECTED), "skynet root must sum every node");

    Measurement {
        benchmark: "skynet",
        messages: EXPECTED as u64 - 1,
        elapsed,
        shards: 1,
    }
}
