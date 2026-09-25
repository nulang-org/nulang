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
use crate::runtime::{
    Mailbox, Message, MessagePayload, MessagePriority, Runtime, RuntimeVmCallbacks,
};
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
    println!(
        "[cross-bench] runtime=nulang benchmark={name} messages={messages} elapsed_ns={}",
        elapsed.as_nanos()
    );
}

/// Report a Nulang-only A/B probe. These records are intentionally distinct
/// from `[cross-bench]` so the Rust/Go/Erlang comparison remains limited to
/// matched workloads.
fn report_ab(name: &str, operations: u64, elapsed: std::time::Duration) {
    println!(
        "[ab-bench] benchmark={name} operations={operations} elapsed_ns={}",
        elapsed.as_nanos()
    );
}

fn ab_noop_handler(_actor: &mut crate::runtime::Actor, _args: &[Value]) {}

#[cfg(feature = "native-codegen")]
fn compile_ab_module(source: &str, name: &str) -> crate::bytecode::CodeModule {
    let tokens = Lexer::new(source).lex().expect("bench: lex failed");
    let ast = Parser::new(tokens)
        .parse_module()
        .expect("bench: parse failed");
    let mut tc = TypeChecker::new();
    tc.check_module(&ast).expect("bench: typecheck failed");
    let hir = crate::hir_lower::lower_module(&ast, &tc.inferred_decl_types);
    let mut mir = crate::mir_lower::lower_module(&hir).expect("bench: MIR lower failed");
    crate::mir_codegen::compile_mir(&mut mir, name).expect("bench: codegen failed")
}

/// Lower-bound local mailbox admission probe for one inline primitive value.
///
/// This intentionally bypasses actor lookup, routing/grain checks, ORCA
/// pointer handling, ready-state publication, and receive-wait wake logic.
/// Comparing it with `enqueue_payload_1` on the same host quantifies how much
/// of local-send cost lives above the mailbox itself before changing runtime
/// semantics.
#[test]
fn bench_ab_mailbox_push_inline_1() {
    const N: usize = 100_000;

    let mut mailbox = Mailbox::new(0);
    let payload = [Value::int(1)];

    let start = Instant::now();
    for _ in 0..N {
        mailbox
            .push_local(Message {
                behavior_id: 0,
                payload: MessagePayload::from_slice(&payload),
                sender: 0,
                priority: MessagePriority::Normal,
                trace_id: None,
            })
            .expect("unbounded benchmark mailbox must admit message");
    }
    let elapsed = start.elapsed();

    assert_eq!(mailbox.len(), N, "mailbox probe must admit every message");
    report_ab("mailbox_push_inline_1", N as u64, elapsed);
}

/// Local enqueue hot-path sweep around the inline-payload boundary.
///
/// 0/1/4 values are the common small-message cases; 5 crosses the proposed
/// four-value inline threshold; 16 is a larger shared-payload control. The
/// timed body includes message construction, mailbox admission, and ready-queue
/// publication but excludes handler execution.
#[test]
fn bench_ab_enqueue_payload_sweep() {
    const N: usize = 100_000;

    for arity in [0usize, 1, 4, 5, 16] {
        let mut rt = Runtime::new();
        let actor_id = rt.spawn_actor(Box::new(Vec::new));
        rt.actors
            .get_mut(&actor_id)
            .expect("spawned actor")
            .register_behavior("handle", ab_noop_handler);
        // Remove spawn-time ready state so the first measured send starts from
        // the same idle actor state for every arity.
        rt.run_scheduler();

        let args: Vec<Value> = (0..arity).map(|i| Value::int(i as i64)).collect();
        let start = Instant::now();
        for _ in 0..N {
            rt.send_message_by_id(actor_id, 0, &args);
        }
        let elapsed = start.elapsed();

        assert_eq!(
            rt.actors
                .get(&actor_id)
                .expect("actor still live")
                .mailbox
                .len(),
            N,
            "enqueue probe must admit every message"
        );
        report_ab(&format!("enqueue_payload_{arity}"), N as u64, elapsed);

        // Drain outside the timed region so every iteration also exercises
        // valid handler delivery and leaves no queued work behind.
        rt.run_scheduler();
        assert!(rt
            .actors
            .get(&actor_id)
            .expect("actor still live")
            .mailbox
            .is_empty());
    }
}

#[cfg(feature = "native-codegen")]
fn run_ab_jit_warm_execution_probe() {
    const REPEATS: usize = 10;
    const TRIPS: usize = 100_000;

    let workloads = [
        (
            "jit_warm_hot_loop_100k",
            format!(
                "var sum = 0; var i = 0; while i < {TRIPS} {{ sum = sum + i * 3 - i / 7; i = i + 1; }}; sum"
            ),
        ),
        (
            "jit_warm_call_loop_100k",
            format!(
                "fn add(x: Int, y: Int) -> Int {{ x + y }}; var sum = 0; var i = 0; while i < {TRIPS} {{ sum = add(sum, i); i = i + 1; }}; sum"
            ),
        ),
    ];

    for (name, source) in workloads {
        let module = compile_ab_module(&source, "bench-ab-jit-warm");

        let mut interp = VM::new_without_jit();
        interp.load_module(module.clone());
        let expected = interp
            .run()
            .expect("bench: interpreter oracle failed")
            .as_int()
            .expect("bench: warm JIT workload must return Int");

        let mut vm = VM::new();
        vm.load_module(module);
        let warm = vm
            .run()
            .expect("bench: JIT warm-up failed")
            .as_int()
            .expect("bench: warm JIT workload must return Int");
        assert_eq!(
            warm, expected,
            "warm-up JIT result must match interpreter oracle"
        );
        assert!(
            vm.jit_compiled_count() > 0,
            "warm execution benchmark must actually compile a JIT region"
        );

        let start = Instant::now();
        let mut last = None;
        for _ in 0..REPEATS {
            last = Some(
                vm.run()
                    .expect("bench: warmed JIT run failed")
                    .as_int()
                    .expect("bench: warm JIT workload must return Int"),
            );
        }
        let elapsed = start.elapsed();

        assert_eq!(
            last,
            Some(expected),
            "warmed JIT result must stay identical to interpreter oracle"
        );
        report_ab(name, REPEATS as u64, elapsed);
    }
}


#[cfg(feature = "native-codegen")]
#[test]
fn bench_ab_jit_tiering_crossover() {
    const REPEATS: usize = 20;

    for trips in [3_000usize, 4_000, 5_000, 7_500] {
        let source = format!(
            "var sum = 0; var i = 0; while i < {trips} {{ sum = sum + i * 3 - i / 7; i = i + 1; }}; sum"
        );
        let module = compile_ab_module(&source, "bench-ab-tiering");

        let mut interp_vms: Vec<VM> = (0..REPEATS)
            .map(|_| {
                let mut vm = VM::new_without_jit();
                vm.load_module(module.clone());
                vm
            })
            .collect();
        let interp_start = Instant::now();
        let mut interp_result = None;
        for vm in &mut interp_vms {
            interp_result = Some(vm.run().expect("bench: interpreter run failed"));
        }
        let interp_elapsed = interp_start.elapsed();

        let mut jit_vms: Vec<VM> = (0..REPEATS)
            .map(|_| {
                let mut vm = VM::new();
                vm.load_module(module.clone());
                vm
            })
            .collect();
        let jit_start = Instant::now();
        let mut jit_result = None;
        for vm in &mut jit_vms {
            jit_result = Some(vm.run().expect("bench: first-run JIT failed"));
        }
        let jit_elapsed = jit_start.elapsed();

        assert_eq!(
            interp_result.and_then(|value| value.as_int()),
            jit_result.and_then(|value| value.as_int()),
            "tiering crossover probe must preserve interpreter/JIT parity"
        );
        report_ab(
            &format!("tier_interp_{trips}"),
            REPEATS as u64,
            interp_elapsed,
        );
        report_ab(
            &format!("tier_jit_first_{trips}"),
            REPEATS as u64,
            jit_elapsed,
        );
    }
    // Keep warmed execution controls inside this already-established A/B test
    // so the same cargo test filter that emits the tiering records cannot
    // silently omit the direct-transition signal.
    run_ab_jit_warm_execution_probe();

}

#[cfg(feature = "native-codegen")]
#[test]
fn bench_ab_aot_actor_drain() {
    use crate::effect_checker::{CapContext, CapabilityAnalyzer, EffectChecker};

    const WARMUP: usize = 2_000;
    const N: usize = 50_000;
    let source = r#"
        actor Counter {
            state total: Int = 0
            behavior Add(n: Int) { self.total = self.total + n }
        }
        fn main() { 0 }
    "#;

    let tokens = Lexer::new(source).lex().expect("bench: lex failed");
    let ast = Parser::new(tokens)
        .parse_module()
        .expect("bench: parse failed");
    let mut tc = TypeChecker::new();
    tc.check_module(&ast).expect("bench: typecheck failed");
    let mut ec = EffectChecker::new();
    ec.check_module(&ast.decls)
        .expect("bench: effect check failed");
    let mut ca = CapabilityAnalyzer::new();
    let ctx = CapContext::new();
    for decl in crate::effect_checker::flatten_decls(&ast.decls) {
        if let crate::ast::Decl::Function { body, .. } = decl {
            ca.infer_cap(&ctx, body)
                .expect("bench: capability analysis failed");
        }
    }

    let hir = crate::hir_lower::lower_module(&ast, &tc.inferred_decl_types);
    let mut mir = crate::mir_lower::lower_module(&hir).expect("bench: MIR lower failed");
    let aot = crate::aot::AotModule::compile(&mir).expect("bench: AOT compile failed");
    let code =
        crate::mir_codegen::compile_mir(&mut mir, "bench-ab-aot").expect("bench: codegen failed");

    // Matched bytecode/JIT path. Warm past the current tier-up threshold so
    // the timed drain represents steady-state tiered execution rather than
    // first-run compilation cost. Enqueue remains outside the timed region,
    // matching the AOT measurement below.
    let mut bytecode_rt = Runtime::new();
    let bytecode_actor = bytecode_rt
        .spawn_from_module(&code, 0, Vec::new())
        .as_actor_id()
        .expect("bench: bytecode actor spawn failed");
    for _ in 0..WARMUP {
        bytecode_rt.send_message_by_id(bytecode_actor, 0, &[Value::int(1)]);
    }
    bytecode_rt.run_scheduler();
    bytecode_rt
        .actors
        .get_mut(&bytecode_actor)
        .expect("bytecode actor live after warmup")
        .set_state_field("total", Value::int(0));

    for _ in 0..N {
        bytecode_rt.send_message_by_id(bytecode_actor, 0, &[Value::int(1)]);
    }
    let bytecode_start = Instant::now();
    bytecode_rt.run_scheduler();
    let bytecode_elapsed = bytecode_start.elapsed();

    let bytecode_total = bytecode_rt
        .actors
        .get(&bytecode_actor)
        .and_then(|actor| actor.get_state_field("total"))
        .and_then(|value| value.as_int());
    assert_eq!(
        bytecode_total,
        Some(N as i64),
        "warmed bytecode/JIT actor must process every message"
    );
    report_ab("bytecode_actor_drain_warm", N as u64, bytecode_elapsed);

    // AOT path over the exact same source and bytecode companion module.
    let mut aot_rt = Runtime::new();
    aot_rt.register_aot_module(aot);
    let aot_actor = aot_rt
        .spawn_from_module(&code, 0, Vec::new())
        .as_actor_id()
        .expect("bench: AOT actor spawn failed");

    // One untimed delivery verifies native wiring and warms non-codegen
    // runtime state; AOT itself has no tier-up phase.
    aot_rt.send_message_by_id(aot_actor, 0, &[Value::int(1)]);
    aot_rt.run_scheduler();
    aot_rt
        .actors
        .get_mut(&aot_actor)
        .expect("AOT actor live after warmup")
        .set_state_field("total", Value::int(0));

    for _ in 0..N {
        aot_rt.send_message_by_id(aot_actor, 0, &[Value::int(1)]);
    }
    let aot_start = Instant::now();
    aot_rt.run_scheduler();
    let aot_elapsed = aot_start.elapsed();

    let aot_total = aot_rt
        .actors
        .get(&aot_actor)
        .and_then(|actor| actor.get_state_field("total"))
        .and_then(|value| value.as_int());
    assert_eq!(
        aot_total,
        Some(N as i64),
        "AOT actor must process every message"
    );
    report_ab("aot_actor_drain", N as u64, aot_elapsed);

    let bytecode_ns = bytecode_elapsed.as_nanos() as f64;
    let aot_ns = aot_elapsed.as_nanos() as f64;
    println!(
        "[backend-bench] workload=actor_drain messages={N} bytecode_jit_ns={} aot_ns={} aot_speedup_x={:.3}",
        bytecode_elapsed.as_nanos(),
        aot_elapsed.as_nanos(),
        bytecode_ns / aot_ns
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
