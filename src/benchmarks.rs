//! Savina-style actor performance benchmarks (borrow P0).
//!
//! These are *throughput* harnesses, not correctness tests. Each compiles a
//! `.nula` actor program through the full pipeline (lex → parse → typecheck →
//! HIR → MIR → bytecode), drives it on a real `Runtime`, measures wall-clock
//! message throughput, and asserts the *result* (a count or sum) so a change
//! that alters semantics still fails the test. Timing is reported, never
//! asserted (CI machines vary too much for timing gates).
//!
//! The five patterns mirror the Savina actor benchmark suite (Imam & Sarkar,
//! 2014): ping-pong, counting, thread-ring, fork-join, and skynet.
//!
//! For meaningful numbers, run under the release profile:
//!
//! ```text
//! cargo test --release --bench benchmarks -- --nocapture
//! ```

use std::cell::RefCell;
use std::rc::Rc;
use std::time::Instant;

use crate::lexer::Lexer;
use crate::parser::Parser;
use crate::runtime::{Runtime, RuntimeVmCallbacks};
use crate::typechecker::TypeChecker;
use crate::vm::{Value, VM};

/// Compile `source` through the full pipeline, attach `runtime` as the actor
/// callback host, and run the top-level expression (which typically spawns
/// actors and returns an actor reference).
fn compile_run_with_runtime(source: &str, runtime: Rc<RefCell<Runtime>>) -> Value {
    let mut lexer = Lexer::new(source);
    let tokens = lexer.lex().expect("bench: lex failed");
    let mut parser = Parser::new(tokens);
    let ast = parser.parse_module().expect("bench: parse failed");
    let mut type_checker = TypeChecker::new();
    type_checker
        .check_module(&ast)
        .expect("bench: typecheck failed");
    let hir = crate::hir_lower::lower_module(&ast, &type_checker.inferred_decl_types);
    let mut mir = crate::mir_lower::lower_module(&hir).expect("bench: MIR lower failed");
    let module = crate::mir_codegen::compile_mir(&mut mir, "bench").expect("bench: codegen failed");
    let mut vm = VM::new();
    vm.load_module(module);
    vm.set_actor_callbacks(Box::new(RuntimeVmCallbacks::new(runtime)));
    vm.run().expect("bench: VM run failed")
}

/// Report measured throughput for one benchmark run.
fn report(name: &str, messages: u64, elapsed: std::time::Duration) {
    let secs = elapsed.as_secs_f64();
    let msg_per_s = messages as f64 / secs;
    let ns_per_msg = elapsed.as_nanos() as f64 / messages as f64;
    println!(
        "[bench] {name}: {messages} msgs in {secs:.3}s = {msg_per_s:.0} msg/s ({ns_per_msg:.1} ns/msg)"
    );
}

/// Counting: one actor, main thread floods it with N messages.
/// Measures single-actor mailbox throughput + scheduler drain.
#[test]
fn bench_counting() {
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
    report("counting", N as u64, elapsed);
}

/// Ping-pong: two actors exchange N round trips via behavior-internal `send`.
/// The wiring (`setup`) phase is untimed; only the round-trip phase is measured.
#[test]
fn bench_ping_pong() {
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

    // Untimed wiring phase: deliver the two `setup` messages.
    rt.borrow_mut().run_scheduler();

    // Timed phase: kick off N round trips.
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
    // 1 kickoff + N ping sends + N pong sends = 2N + 1 messages.
    report("ping_pong", 2 * N as u64 + 1, elapsed);
}

/// Thread-ring: a token circles R actors H hops, then reports to a sink.
/// The ring is wired in an untimed setup phase; only the H-hop phase is timed.
#[test]
fn bench_thread_ring() {
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
    // The Sink is spawned first, so it holds the smallest (monotonic) id.
    let sink = {
        let rt = rt.borrow();
        *rt.actors.keys().min().expect("sink spawned first")
    };

    // Untimed wiring phase.
    rt.borrow_mut().run_scheduler();

    // Timed phase: H hops.
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
    report("thread_ring", HOPS as u64, elapsed);
}

/// Fork-join: main fans out F tasks round-robin to W workers; each worker
/// acks a sink. Measures fan-out + aggregation throughput.
#[test]
fn bench_fork_join() {
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

    // Untimed wiring phase: deliver the `wire_worker` messages.
    rt.borrow_mut().run_scheduler();

    let start = Instant::now();
    for i in 0..TASKS {
        let w = worker_ids[(i as usize) % WORKERS];
        rt.borrow_mut()
            .send_message(w, "task", &[Value::int(i as i64)]);
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
    // TASKS fan-out sends + TASKS acks.
    report("fork_join", 2 * TASKS as u64, elapsed);
}

/// Skynet: a 10-ary tree of depth DEPTH. Each leaf returns 1; each internal
/// node sums its 10 children plus 1. The root's total is the node count,
/// `(10^(DEPTH+1) - 1) / 9`. Measures actor-creation rate + tree aggregation.
///
/// Depth is capped at 3 (1111 actors): Nulang's per-actor 16 KiB heap (with
/// equal-size growth chaining) makes the canonical 1M-leaf skynet (~16 GiB
/// of heap) infeasible — a cost this benchmark surfaces by construction
/// rather than hiding.
#[test]
fn bench_skynet() {
    const DEPTH: i64 = 3;
    const EXPECTED: i64 = 1111; // (10^4 - 1) / 9

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
    // Each node (except the root) sends exactly one `result` up: 1110 messages.
    report("skynet", EXPECTED as u64 - 1, elapsed);
}
