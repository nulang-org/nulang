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
}

#[derive(Debug)]
struct Measurement {
    benchmark: &'static str,
    messages: u64,
    elapsed: Duration,
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
    }))
}

fn print_usage() {
    eprintln!(
        "Usage: nulang-savina [--format human|jsonl] [--benchmark NAME] [--repeat N] [--list]"
    );
}

fn emit(measurement: &Measurement, iteration: u32, format: OutputFormat) {
    match format {
        OutputFormat::Human => {
            println!(
                "[bench] {}: {} msgs in {:.3}s = {:.0} msg/s ({:.1} ns/msg)",
                measurement.benchmark,
                measurement.messages,
                measurement.elapsed.as_secs_f64(),
                measurement.messages_per_second(),
                measurement.ns_per_message()
            );
            println!(
                "[cross-bench] runtime=nulang benchmark={} messages={} elapsed_ns={}",
                measurement.benchmark,
                measurement.messages,
                measurement.elapsed.as_nanos()
            );
        }
        OutputFormat::Jsonl => {
            let elapsed_ns = u64::try_from(measurement.elapsed.as_nanos())
                .expect("benchmark duration must fit in u64 nanoseconds");
            println!(
                "{}",
                json!({
                    "schema": 1,
                    "runtime": "nulang",
                    "suite": "savina-style",
                    "benchmark": measurement.benchmark,
                    "iteration": iteration,
                    "messages": measurement.messages,
                    "elapsed_ns": elapsed_ns,
                    "messages_per_second": measurement.messages_per_second(),
                    "ns_per_message": measurement.ns_per_message(),
                })
            );
        }
    }
}

fn compile_run_with_runtime(source: &str, runtime: Rc<RefCell<Runtime>>) -> Value {
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
    let module =
        nulang::mir_codegen::compile_mir(&mut mir, "savina").expect("bench: codegen failed");
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
    }
}
