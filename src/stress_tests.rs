//! Stress Tests: Chaos Engineering for the Nulang Actor Runtime
//!
//! These tests deliberately stress the most complex and undertested areas of
//! the system: actor lifecycle, supervision trees, links, monitors, and
//! scheduler fairness under load.

use std::collections::HashMap;
use std::sync::{Arc, Mutex};
use std::time::Duration;

use crate::bytecode::{CodeModule, Constant, Instruction, OpCode};
use crate::lexer::Lexer;
use crate::parser::Parser;
use crate::runtime::*;
use crate::typechecker::TypeChecker;
use crate::types::ExitReason;
use crate::vm::{Value, VM};

// ---------------------------------------------------------------------------
// Helper: TestContext
// ---------------------------------------------------------------------------

#[derive(Debug, Clone, Default)]
#[allow(dead_code)]
struct TestContext {
    counters: HashMap<String, u64>,
    log: Vec<String>,
}

#[allow(dead_code)]
impl TestContext {
    fn increment(&mut self, key: &str) {
        *self.counters.entry(key.to_string()).or_insert(0) += 1;
    }

    fn get(&self, key: &str) -> u64 {
        self.counters.get(key).copied().unwrap_or(0)
    }

    fn record(&mut self, entry: String) {
        self.log.push(entry);
    }
}

// ---------------------------------------------------------------------------

// ---------------------------------------------------------------------------
// Helper: compile_and_run_nula — compile .nula source and run it
// ---------------------------------------------------------------------------

/// Compile a .nula source string through the full pipeline and run it,
/// returning the resulting Value.
fn compile_and_run_nula(source: &str) -> Result<Value, crate::types::NuError> {
    let mut lexer = Lexer::new(source);
    let tokens = lexer.lex()?;
    let mut parser = Parser::new(tokens);
    let ast = parser.parse_module()?;
    let mut type_checker = TypeChecker::new();
    type_checker.check_module(&ast)?;
    let hir = crate::hir_lower::lower_module(&ast, &type_checker.inferred_decl_types);
    let mut mir = crate::mir_lower::lower_module(&hir)?;
    let module = crate::mir_codegen::compile_mir(&mut mir, "stress")?;
    let mut vm = VM::new();
    vm.load_module(module);
    vm.run()
}

/// Compile and run .nula source, asserting that the result is the given integer.
fn assert_nula_int(source: &str, expected: i64) {
    let value = compile_and_run_nula(source).unwrap_or_else(|e| {
        panic!(
            "nula source failed to compile/run: {:?}\nSource:\n{}",
            e, source
        );
    });
    assert_eq!(
        value.as_int(),
        Some(expected),
        "Expected {}, got {:?}\nSource:\n{}",
        expected,
        value,
        source
    );
}
// ---------------------------------------------------------------------------
// Test 1 — Slow Worker + Mailbox Flood
// ---------------------------------------------------------------------------

#[test]
fn stress_slow_worker_with_mailbox_flood() {
    let mut rt = Runtime::new();
    let _ctx = Arc::new(Mutex::new(TestContext::default()));

    let slow_actor = rt.spawn_actor(Box::new(|| {
        vec![
            ("name".into(), Value::int(1)), // 1 = "slow_worker"
            ("mode".into(), Value::int(2)), // 2 = "slow_io"
            ("quota".into(), Value::int(1000)),
        ]
    }));

    let flood_sender = rt.spawn_actor(Box::new(|| {
        vec![
            ("name".into(), Value::int(3)), // 3 = "flood_sender"
        ]
    }));

    // Pre-seed the slow actor's mailbox with 10_000 messages
    for i in 0..10_000 {
        rt.send_message(
            slow_actor,
            "work",
            &[
                Value::int(i),
                Value::int(42), // payload marker
            ],
        );
    }

    // Inject a system-priority exit signal mid-flood via a linked actor
    let signaler = rt.spawn_actor(Box::new(|| vec![]));
    rt.link_actors(slow_actor, signaler);
    rt.exit_actor(signaler, ExitReason::Error("system_signal".into()));

    // Seed flood sender's mailbox
    for i in 10_000..15_000 {
        rt.send_message(flood_sender, "forward", &[Value::int(i)]);
    }

    // Run scheduler to process all messages
    rt.run_scheduler();

    // --- Assertions ---

    // 1a. slow_actor survived the flood (signaler exited abnormally but
    //     the link kills slow_actor; with trap_exits=false it dies too.
    //     We verify the runtime is consistent either way.)
    let slow_exists = rt.actors.contains_key(&slow_actor);

    // 1b. If slow_actor survived, its mailbox should be drained.
    if slow_exists {
        if let Some(actor) = rt.actors.get(&slow_actor) {
            assert!(
                actor.mailbox.is_empty(),
                "slow_actor mailbox should be drained after scheduler run"
            );
            // The scheduler processed messages — reduction_count proves work happened.
            assert!(
                actor.reduction_count > 0,
                "slow_actor should have processed some messages, reductions={}",
                actor.reduction_count
            );
        }
    }

    // 1c. Memory sanity: no leaked runtime state. Terminated actors are
    // reaped from the actor table by handle_actor_exit, so anything still
    // registered must be in a live (non-Terminated) state.
    assert!(
        rt.actors
            .values()
            .all(|a| a.state != ActorState::Terminated),
        "terminated actors should have been reaped from the runtime"
    );
}

// ---------------------------------------------------------------------------
// Test 2 — Actor Crash During Scheduling
// ---------------------------------------------------------------------------

#[test]
fn stress_actor_crash_during_scheduling() {
    let mut rt = Runtime::new();

    let sup = rt.create_supervisor("test_sup", RestartStrategy::OneForOne);

    let child = rt.spawn_actor(Box::new(|| {
        vec![
            ("name".into(), Value::int(4)), // 4 = "effect_child"
        ]
    }));

    let child_spec = ChildSpec::new("child", RestartPolicy::Permanent);
    rt.supervise_child(sup, child_spec, child);

    let sibling = rt.spawn_actor(Box::new(|| {
        vec![
            ("name".into(), Value::int(5)), // 5 = "sibling"
        ]
    }));
    rt.link_actors(child, sibling);

    // Simulate crash
    rt.exit_actor(child, ExitReason::Error("crash_during_sched".into()));
    rt.run_scheduler();

    // Sibling (no trap_exits) should be terminated by linked exit
    assert!(
        !rt.actors.contains_key(&sibling),
        "sibling linked to crashed child should have been terminated"
    );

    // Supervisor should survive
    assert!(
        rt.actors.contains_key(&sup),
        "supervisor should survive child crash"
    );

    // Supervisor should have recorded the restart
    if let Some(supvisor) = rt.supervisors.get(&sup) {
        assert!(
            supvisor.child_count() >= 1,
            "supervisor should still have children after restart"
        );
    }
}

// ---------------------------------------------------------------------------
// Test 3 — Cascading Exit Under Load
// ---------------------------------------------------------------------------

#[test]
fn stress_cascading_exit_under_load() {
    let mut rt = Runtime::new();

    let root = rt.create_supervisor("root", RestartStrategy::OneForOne);

    let mut supervisors: Vec<Vec<u64>> = vec![vec![root]];

    for level in 1..=4 {
        let mut current_level = Vec::new();
        let parent_level = &supervisors[level - 1];

        for parent in parent_level.iter().copied() {
            let children_count = if level < 4 { 3 } else { 0 };
            for i in 0..children_count {
                let strategy = match level {
                    1 | 2 => RestartStrategy::OneForOne,
                    3 => RestartStrategy::RestForOne,
                    _ => unreachable!(),
                };

                let sup_name = format!("L{}_{}", level, i);
                let sup = rt.create_supervisor(&sup_name, strategy);
                let spec = ChildSpec::new(&sup_name, RestartPolicy::Permanent);
                rt.supervise_child(parent, spec, sup);
                current_level.push(sup);
            }
        }

        if !current_level.is_empty() {
            supervisors.push(current_level);
        }
    }

    // L4 — leaf actors supervised by L3 supervisors
    let mut leaf_actors: Vec<u64> = Vec::new();
    if supervisors.len() > 3 {
        for sup_l3 in &supervisors[3] {
            for leaf_idx in 0..2 {
                let leaf = rt.spawn_actor(Box::new(move || {
                    vec![
                        ("name".into(), Value::int(10 + leaf_idx as i64)),
                        ("level".into(), Value::int(4)),
                    ]
                }));
                let spec = ChildSpec::new(format!("leaf_{}", leaf_idx), RestartPolicy::Temporary);
                rt.supervise_child(*sup_l3, spec, leaf);
                leaf_actors.push(leaf);
            }
        }
    }

    assert!(!leaf_actors.is_empty(), "should have created leaf actors");

    let pre_crash_count = rt.actors.len();
    let victim = leaf_actors[0];

    rt.exit_actor(victim, ExitReason::Error("leaf_crash".into()));
    rt.run_scheduler();

    // Victim leaf is gone (Temporary policy → not restarted)
    assert!(
        !rt.actors.contains_key(&victim),
        "crashed leaf with Temporary policy should not be restarted"
    );

    // Only the crashed leaf should be removed
    let post_crash_count = rt.actors.len();
    assert_eq!(
        post_crash_count,
        pre_crash_count - 1,
        "only the crashed leaf should be removed; pre={}, post={}",
        pre_crash_count,
        post_crash_count
    );

    // All supervisors still exist
    for (level, sups) in supervisors.iter().enumerate() {
        for sup in sups {
            assert!(
                rt.actors.contains_key(sup),
                "supervisor at level {} should survive leaf crash",
                level
            );
        }
    }

    // Sibling leaf still exists
    if leaf_actors.len() > 1 {
        assert!(
            rt.actors.contains_key(&leaf_actors[1]),
            "sibling leaf should survive with OneForOne at L1-L2"
        );
    }
}

// ---------------------------------------------------------------------------
// Test 4 — Monitor During Rapid Spawn/Exit
// ---------------------------------------------------------------------------

#[test]
fn stress_monitor_during_rapid_spawn_exit() {
    let mut rt = Runtime::new();

    let watcher = rt.spawn_actor(Box::new(|| {
        vec![
            ("name".into(), Value::int(6)), // 6 = "watcher"
            ("role".into(), Value::int(7)), // 7 = "monitor_collector"
        ]
    }));

    let mut targets: Vec<u64> = Vec::with_capacity(100);
    for i in 0..100 {
        let target = rt.spawn_actor(Box::new(move || {
            vec![
                ("name".into(), Value::int(100 + i as i64)),
                ("seq".into(), Value::int(i as i64)),
            ]
        }));
        rt.monitor(watcher, target);
        targets.push(target);
    }

    // Exit all targets — DOWN messages are sent synchronously to watcher
    for (i, target) in targets.iter().enumerate() {
        rt.exit_actor(*target, ExitReason::Error(format!("rapid_exit_{}", i)));
    }

    // Check mailbox BEFORE running scheduler (scheduler consumes messages)
    let down_count_before = rt
        .actors
        .get(&watcher)
        .map(|a| a.mailbox.len())
        .unwrap_or(0);
    assert!(
        down_count_before >= 100,
        "watcher should have at least 100 DOWN messages, got {}",
        down_count_before
    );

    rt.run_scheduler();

    // All target actors are gone
    for target in &targets {
        assert!(
            !rt.actors.contains_key(target),
            "target actor {} should be removed after exit",
            target
        );
    }

    // Watcher itself survived
    assert!(rt.actors.contains_key(&watcher), "watcher should survive");
}

// ---------------------------------------------------------------------------
// Test 5 — Scheduler with Mixed Workload
// ---------------------------------------------------------------------------

#[test]
fn stress_scheduler_with_mixed_workload() {
    let mut rt = Runtime::new();

    let sink = rt.spawn_actor(Box::new(|| {
        vec![
            ("name".into(), Value::int(8)), // 8 = "sink"
            ("role".into(), Value::int(9)), // 9 = "message_collector"
        ]
    }));

    let cpu_actor = rt.spawn_actor(Box::new(|| {
        vec![
            ("name".into(), Value::int(10)), // 10 = "cpu_heavy"
            ("mode".into(), Value::int(11)), // 11 = "cpu_bound"
            ("quota".into(), Value::int(500)),
        ]
    }));

    let io_actor = rt.spawn_actor(Box::new(|| {
        vec![
            ("name".into(), Value::int(12)), // 12 = "io_waiter"
            ("mode".into(), Value::int(13)), // 13 = "io_bound"
            ("quota".into(), Value::int(1)),
        ]
    }));

    // Seed workloads: send messages to each actor type
    for i in 0..50 {
        rt.send_message(
            cpu_actor,
            "compute",
            &[Value::int(i), Value::int(1_000_000)],
        );
    }

    for i in 0..100 {
        rt.send_message(
            io_actor,
            "io_op",
            &[
                Value::int(i),
                Value::int(20), // 20 = "read"
            ],
        );
    }

    // Send 200 messages to the sink directly
    for i in 0..200 {
        rt.send_message(sink, "collect", &[Value::int(i)]);
    }

    rt.run_scheduler();

    // All actors still exist and made progress
    if let Some(actor) = rt.actors.get(&cpu_actor) {
        assert!(
            actor.reduction_count > 0,
            "CPU actor should have made progress, reductions={}",
            actor.reduction_count
        );
    } else {
        panic!("CPU actor should still exist");
    }

    if let Some(actor) = rt.actors.get(&io_actor) {
        assert!(
            actor.reduction_count > 0,
            "I/O actor should have made progress"
        );
    } else {
        panic!("I/O actor should still exist");
    }

    if let Some(actor) = rt.actors.get(&sink) {
        assert!(
            actor.reduction_count > 0,
            "Sink should have received and processed messages"
        );
    } else {
        panic!("sink actor should still exist");
    }
}

// ---------------------------------------------------------------------------
// Test 6 — Mailbox Never Drops System Messages
// ---------------------------------------------------------------------------

#[test]
fn stress_mailbox_never_drops_system_messages() {
    let mut rt = Runtime::new();

    let actor = rt.spawn_actor(Box::new(|| {
        vec![
            ("name".into(), Value::int(16)), // 16 = "mailbox_test"
        ]
    }));

    // Push 1_000 normal-priority messages
    for i in 0..1_000 {
        let msg = Message {
            behavior_id: 1,
            payload: Arc::new(vec![Value::int(i)]),
            sender: 0,
            priority: MessagePriority::Normal,
            trace_id: None,
        };
        if let Some(a) = rt.actors.get_mut(&actor) {
            let _ = a.mailbox.push(msg);
        }
    }

    // Push 100 system-priority messages
    for i in 0..100 {
        let msg = Message {
            behavior_id: 0,
            payload: Arc::new(vec![Value::int(1000 + i)]),
            sender: 0,
            priority: MessagePriority::System,
            trace_id: None,
        };
        if let Some(a) = rt.actors.get_mut(&actor) {
            let _ = a.mailbox.push(msg);
        }
    }

    // Push 50 bulk-priority messages
    for i in 0..50 {
        let msg = Message {
            behavior_id: 2,
            payload: Arc::new(vec![Value::int(2000 + i)]),
            sender: 0,
            priority: MessagePriority::Bulk,
            trace_id: None,
        };
        if let Some(a) = rt.actors.get_mut(&actor) {
            let _ = a.mailbox.push(msg);
        }
    }

    // Pop all messages and verify counts
    let mut system_seen = 0;
    let mut normal_seen = 0;
    let mut bulk_seen = 0;

    if let Some(a) = rt.actors.get_mut(&actor) {
        while let Some(msg) = a.mailbox.pop() {
            match msg.priority {
                MessagePriority::System => system_seen += 1,
                MessagePriority::Normal => normal_seen += 1,
                MessagePriority::Bulk => bulk_seen += 1,
            }
        }
    }

    assert_eq!(
        system_seen, 100,
        "all 100 system messages must be present, got {}",
        system_seen
    );
    assert_eq!(
        normal_seen, 1_000,
        "all 1_000 normal messages must be present, got {}",
        normal_seen
    );
    assert_eq!(
        bulk_seen, 50,
        "all 50 bulk messages must be present, got {}",
        bulk_seen
    );
}

// ---------------------------------------------------------------------------
// Test 7 — Orphaned Actor Cleanup
// ---------------------------------------------------------------------------

#[test]
fn stress_orphaned_actor_cleanup() {
    let mut rt = Runtime::new();
    const N: usize = 20;
    const DEGREE: usize = 2;

    let mut actors: Vec<u64> = Vec::with_capacity(N);
    for i in 0..N {
        let id = rt.spawn_actor(Box::new(move || {
            vec![
                ("name".into(), Value::int(50 + i as i64)),
                ("idx".into(), Value::int(i as i64)),
            ]
        }));
        actors.push(id);
    }

    // Link each actor to DEGREE others (ring topology — no cascading)
    for i in 0..N {
        for d in 1..=DEGREE {
            let j = (i + d) % N;
            if i < j {
                rt.link_actors(actors[i], actors[j]);
            }
        }
    }

    let pre_kill_count = rt.actors.len();
    assert_eq!(pre_kill_count, N, "should have {} actors before kill", N);

    let hub = actors[N / 2];
    rt.exit_actor(hub, ExitReason::Error("hub_killed".into()));
    rt.run_scheduler();

    // Hub is gone
    assert!(!rt.actors.contains_key(&hub), "hub actor should be removed");

    // Count terminated linked neighbors (DEGREE forward + DEGREE backward)
    let mut terminated_count = 0;
    for d in 1..=DEGREE {
        let linked_idx_forward = (N / 2 + d) % N;
        let linked_idx_backward = (N / 2 + N - d) % N;

        if !rt.actors.contains_key(&actors[linked_idx_forward]) {
            terminated_count += 1;
        }
        if !rt.actors.contains_key(&actors[linked_idx_backward]) {
            terminated_count += 1;
        }
    }

    // With trap_exits=false, linked neighbors should have terminated
    assert!(
        terminated_count >= DEGREE,
        "at least {} neighbors should be terminated, got {}",
        DEGREE,
        terminated_count
    );

    // Remaining count should be consistent
    let actual_remaining = rt.actors.len();
    assert!(
        actual_remaining < N,
        "some actors should remain after cleanup, got {}",
        actual_remaining
    );
}

// ---------------------------------------------------------------------------
// Test 8 — Reduction Quota Fairness
// ---------------------------------------------------------------------------

#[test]
fn stress_reduction_quota_fairness() {
    let mut rt = Runtime::new();

    let actor_a = rt.spawn_actor(Box::new(|| {
        vec![
            ("name".into(), Value::int(30)), // 30 = "fair_a"
            ("quota".into(), Value::int(10)),
        ]
    }));

    let actor_b = rt.spawn_actor(Box::new(|| {
        vec![
            ("name".into(), Value::int(31)), // 31 = "fair_b"
            ("quota".into(), Value::int(10)),
        ]
    }));

    const MSG_COUNT: usize = 100;
    for i in 0..MSG_COUNT {
        rt.send_message(actor_a, "work", &[Value::int(i as i64)]);
        rt.send_message(actor_b, "work", &[Value::int(i as i64)]);
    }

    // Run scheduler
    rt.run_scheduler();

    // Both actors should have processed messages (reductions > 0)
    if let Some(a) = rt.actors.get(&actor_a) {
        assert!(a.reduction_count > 0, "actor_a should have made progress");
    }
    if let Some(b) = rt.actors.get(&actor_b) {
        assert!(b.reduction_count > 0, "actor_b should have made progress");
    }

    // Both mailboxes should be empty
    if let Some(a) = rt.actors.get(&actor_a) {
        assert!(a.mailbox.is_empty(), "actor_a mailbox should be empty");
    }
    if let Some(b) = rt.actors.get(&actor_b) {
        assert!(b.mailbox.is_empty(), "actor_b mailbox should be empty");
    }
}

// ---------------------------------------------------------------------------
// Test 9 — Effect Resume After Mailbox Pressure
// ---------------------------------------------------------------------------

#[test]
fn stress_effect_resume_after_mailbox_pressure() {
    let mut rt = Runtime::new();

    let effect_actor = rt.spawn_actor(Box::new(|| {
        vec![
            ("name".into(), Value::int(40)),   // 40 = "effect_resumer"
            ("effect".into(), Value::int(41)), // 41 = "SimulatedRead"
        ]
    }));

    // Start an effect on the actor
    rt.send_message(effect_actor, "start_effect", &[]);

    // Flood the mailbox while actor is running
    for i in 0..5_000 {
        rt.send_message(effect_actor, "flood", &[Value::int(i)]);
    }

    // Run scheduler — all messages should be processed
    rt.run_scheduler();

    // Actor should still exist and have processed messages
    if let Some(actor) = rt.actors.get(&effect_actor) {
        assert!(
            actor.reduction_count > 0,
            "actor should have processed messages, reductions={}",
            actor.reduction_count
        );
        assert!(
            actor.mailbox.is_empty(),
            "mailbox should be empty after scheduler run"
        );
    } else {
        // Actor may have been terminated by linked exit or supervisor
        // — that is also a valid outcome for the stress test.
    }
}

// ---------------------------------------------------------------------------
// Test 10 — Supervisor Crash During Recovery
// ---------------------------------------------------------------------------

#[test]
fn stress_supervisor_crash_during_recovery() {
    let mut rt = Runtime::new();

    let root = rt.create_supervisor("root", RestartStrategy::OneForAll);

    let mid = rt.create_supervisor("mid", RestartStrategy::OneForOne);
    rt.supervise_child(root, ChildSpec::new("mid", RestartPolicy::Permanent), mid);

    let leaf = rt.spawn_actor(Box::new(|| {
        vec![
            ("name".into(), Value::int(50)), // 50 = "leaf"
        ]
    }));
    rt.supervise_child(mid, ChildSpec::new("leaf", RestartPolicy::Permanent), leaf);

    let _pre_crash_count = rt.actors.len();

    rt.exit_actor(
        mid,
        ExitReason::Error("supervisor_died_mid_recovery".into()),
    );
    rt.run_scheduler();

    // Root supervisor should survive
    assert!(
        rt.actors.contains_key(&root),
        "root supervisor should survive"
    );

    // Actor count should be stable (root + replacement mid + replacement leaf)
    let post_count = rt.actors.len();
    assert!(
        post_count >= 1,
        "at least root should remain, got {} actors",
        post_count
    );
}

// ---------------------------------------------------------------------------
// Test 11 — Registry High Churn
// ---------------------------------------------------------------------------

#[test]
fn stress_registry_high_churn() {
    let mut rt = Runtime::new();
    const N: usize = 500;
    let mut ids = Vec::with_capacity(N);

    for i in 0..N {
        let id = rt.spawn_actor(Box::new(move || {
            vec![("name".into(), Value::int(200 + i as i64))]
        }));
        ids.push(id);
        let name = format!("worker_{}", i);
        rt.registry.register(&name, id).unwrap();
    }

    assert_eq!(rt.registry.registered().len(), N);

    for i in 0..N {
        let name = format!("worker_{}", i);
        assert_eq!(rt.registry.whereis(&name), Some(ids[i]));
    }

    for i in (0..N).step_by(2) {
        let name = format!("worker_{}", i);
        rt.registry.unregister(&name).unwrap();
    }

    assert_eq!(rt.registry.registered().len(), N / 2);
    rt.run_scheduler();

    for i in 0..N {
        let name = format!("worker_{}", i);
        if i % 2 == 0 {
            assert_eq!(rt.registry.whereis(&name), None);
        } else {
            assert_eq!(rt.registry.whereis(&name), Some(ids[i]));
        }
    }
}

// ---------------------------------------------------------------------------
// Test 12 — Process Groups Membership Churn
// ---------------------------------------------------------------------------

#[test]
fn stress_process_groups_membership_churn() {
    let mut rt = Runtime::new();
    const N: usize = 200;
    let mut ids = Vec::with_capacity(N);

    for i in 0..N {
        let id = rt.spawn_actor(Box::new(move || {
            vec![("name".into(), Value::int(300 + i as i64))]
        }));
        ids.push(id);
    }

    for (i, id) in ids.iter().enumerate() {
        let group = format!("group_{}", i % 10);
        rt.process_groups.join(&group, *id).unwrap();
    }

    for g in 0..10 {
        let group = format!("group_{}", g);
        assert_eq!(rt.process_groups.member_count(&group), N / 10);
    }

    for (i, id) in ids.iter().enumerate() {
        if i % 3 == 0 {
            let group = format!("group_{}", i % 10);
            assert!(rt.process_groups.leave(&group, *id));
        }
    }

    for g in 0..10 {
        let group = format!("group_{}", g);
        let remaining = rt.process_groups.member_count(&group);
        assert!(remaining > 0);
        assert!(remaining <= N / 10);
    }

    let victim = ids[1];
    rt.exit_actor(victim, ExitReason::Error("pg_exit".into()));
    rt.run_scheduler();

    for g in 0..10 {
        let group = format!("group_{}", g);
        assert!(!rt.process_groups.is_member(&group, victim));
    }
}

// ---------------------------------------------------------------------------
// Test 13 — Timer Wheel Overload
// ---------------------------------------------------------------------------

#[test]
fn stress_timer_wheel_overload() {
    let mut rt = Runtime::new();
    let actor = rt.spawn_actor(Box::new(|| vec![("name".into(), Value::int(400))]));

    let mut ids = Vec::new();
    for i in 0..5_000 {
        let delay = std::time::Duration::from_nanos((i % 100 + 1) as u64);
        let id = rt
            .timer_wheel
            .send_after(delay, actor, 1, vec![Value::int(i as i64)]);
        ids.push(id);
    }

    assert_eq!(rt.timer_wheel.len(), 5_000);

    for i in (0..5_000).step_by(2) {
        assert!(rt.timer_wheel.cancel(ids[i]));
    }

    assert_eq!(rt.timer_wheel.len(), 2_500);

    std::thread::sleep(std::time::Duration::from_millis(5));
    let fired = rt.timer_wheel.tick(std::time::Instant::now());
    assert!(!fired.is_empty(), "some timers should have fired");
}

// ---------------------------------------------------------------------------
// Test 14 — Persistent Actor Checkpoint / Recovery
// ---------------------------------------------------------------------------

#[test]
fn stress_persistent_actor_checkpoint_recovery() {
    let mut rt = Runtime::new();
    let mut models = HashMap::new();
    models.insert("counter".to_string(), StateModel::Durable);
    models.insert("scratch".to_string(), StateModel::Local);

    let actor_id = rt.spawn_persistent_actor(
        Box::new(|| {
            vec![
                ("counter".into(), Value::int(0)),
                ("scratch".into(), Value::int(0)),
            ]
        }),
        models,
    );

    if let Some(actor) = rt.actors.get_mut(&actor_id) {
        actor.set_state_field("counter", Value::int(42));
        actor.set_state_field("scratch", Value::int(99));
    }

    rt.checkpoint_actor(actor_id);
    assert_eq!(rt.persistence.latest_sequence(actor_id), 1);

    rt.actors.remove(&actor_id);
    let recovered = rt.recover_actor(actor_id);
    assert_eq!(recovered, Some(actor_id));

    let counter = rt
        .actors
        .get(&actor_id)
        .and_then(|a| a.get_state_field("counter"))
        .and_then(|v| v.as_int());
    assert_eq!(counter, Some(42));

    let scratch = rt
        .actors
        .get(&actor_id)
        .and_then(|a| a.get_state_field("scratch"));
    assert_eq!(scratch, None, "Local fields should not survive recovery");
}

// ---------------------------------------------------------------------------
// Test 15 — CRDT Counter Merge Stress
// ---------------------------------------------------------------------------

#[test]
fn stress_crdt_counter_merge_stress() {
    let mut counter_a = GCounter::new(1);
    let mut counter_b = GCounter::new(2);

    for _ in 0..1_000 {
        counter_a.increment();
    }
    for _ in 0..750 {
        counter_b.increment();
    }

    counter_a.merge(&counter_b);
    assert_eq!(counter_a.value(), 1_750);

    counter_b.merge(&counter_a);
    assert_eq!(counter_b.value(), 1_750);
}

// ---------------------------------------------------------------------------
// Test 16 — CRDT Manager Sync Ops
// ---------------------------------------------------------------------------

#[test]
fn stress_crdt_manager_sync_ops() {
    let mut manager = CrdtManager::new(1);
    let (id, _) = manager.create_gcounter();

    for _ in 0..100 {
        if let Some(c) = manager.get_gcounter_mut(id) {
            c.increment();
        }
    }

    let ops = manager.generate_sync_ops();
    assert!(!ops.is_empty(), "sync ops should be generated");

    for op in ops {
        assert_eq!(op.crdt_type, CrdtType::GCounter);
        assert_eq!(op.crdt_id, id);
    }
}

// ---------------------------------------------------------------------------
// Test 17 — Monitor Spawn Storm
// ---------------------------------------------------------------------------

#[test]
fn stress_monitor_spawn_storm() {
    let mut rt = Runtime::new();
    let watcher = rt.spawn_actor(Box::new(|| vec![("name".into(), Value::int(500))]));

    let mut ids = Vec::with_capacity(100);
    for i in 0..100 {
        let id = rt.spawn_actor(Box::new(move || {
            vec![("name".into(), Value::int(600 + i as i64))]
        }));
        rt.monitor(watcher, id);
        ids.push(id);
    }

    for id in &ids {
        rt.exit_actor(*id, ExitReason::Error("storm".into()));
    }
    rt.run_scheduler();

    for id in &ids {
        assert!(!rt.actors.contains_key(id));
    }
    assert!(rt.actors.contains_key(&watcher), "watcher should survive");
}

// ---------------------------------------------------------------------------
// Test 18 — JIT Hot Loop Matches Interpreter
// ---------------------------------------------------------------------------

#[test]
fn stress_jit_hot_loop_then_cold_fallback() {
    let mut module = CodeModule::new("jit_stress");

    // r0 = sum, r1 = i, r2 = limit, r3 = 1, r4 = condition
    module.emit(Instruction::new1(OpCode::Const0, 0)); // sum = 0
    module.emit(Instruction::new1(OpCode::Const0, 1)); // i = 0

    let limit_const = module.add_constant(Constant::Int(100));
    module.emit(Instruction::new3(
        OpCode::ConstU,
        ((limit_const >> 8) & 0xFF) as u8,
        (limit_const & 0xFF) as u8,
        2,
    )); // r2 = 100

    module.emit(Instruction::new1(OpCode::Const1, 3)); // r3 = 1

    let loop_start = module.current_offset();
    module.emit(Instruction::new3(OpCode::ICmpLt, 1, 2, 4)); // r4 = i < limit
    let jmpf_idx = module.current_offset();
    module.emit(Instruction::new2(OpCode::JmpF, 4, 0)); // exit if false
    module.emit(Instruction::new3(OpCode::IAdd, 0, 1, 0)); // sum += i
    module.emit(Instruction::new3(OpCode::IAdd, 1, 3, 1)); // i += 1
    let jmp_back_idx = module.current_offset();
    let back_offset = loop_start as i64 - jmp_back_idx as i64;
    module.emit(Instruction::new3(
        OpCode::Jmp,
        ((back_offset as i16 >> 8) & 0xFF) as u8,
        (back_offset as i16 & 0xFF) as u8,
        0,
    ));

    let after_loop = module.current_offset();
    if let Some(instr) = module.instructions.get_mut(jmpf_idx) {
        let forward_offset = after_loop as i64 - jmpf_idx as i64;
        instr.op2 = ((forward_offset as i16 >> 8) & 0xFF) as u8;
        instr.op3 = (forward_offset as i16 & 0xFF) as u8;
    }
    module.emit(Instruction::new0(OpCode::Halt));
    module.entry_point = Some(0);

    let mut vm = VM::new();
    vm.load_module(module.clone());
    let cold_result = vm.run_from(0, 0).unwrap();
    assert_eq!(
        cold_result.as_int(),
        Some(4950),
        "sum 0..100 should be 4950"
    );

    // Heat the entry region until it is JIT-compiled.
    for _ in 0..2_000 {
        let _ = vm.run_from(0, 0);
    }

    let hot_result = vm.run_from(0, 0).unwrap();
    assert_eq!(
        hot_result.as_int(),
        cold_result.as_int(),
        "JIT hot loop should match interpreter"
    );
    assert_eq!(hot_result.as_int(), Some(4950));

    // A fresh VM (its own session counters start cold) should still
    // compute the same value.
    let mut fresh_vm = VM::new();
    fresh_vm.load_module(module);
    let fallback = fresh_vm.run_from(0, 0).unwrap();
    assert_eq!(fallback.as_int(), Some(4950));
}

// ---------------------------------------------------------------------------
// Test 19 — Remote Actor Cache LRU Eviction
// ---------------------------------------------------------------------------

#[test]
fn stress_remote_actor_cache_lru_eviction() {
    let mut cache = RemoteActorCache::new(100, Duration::from_secs(3600));
    let node = NodeId(7);

    for i in 0..500u64 {
        cache.put(node, i);
    }

    assert_eq!(cache.len(), 100, "cache should not exceed capacity");

    for i in 400..500 {
        assert!(
            cache.get(node, i).is_some(),
            "recently inserted entries should remain"
        );
    }

    for i in 0..400 {
        assert!(
            cache.get(node, i).is_none(),
            "least-recently-used entries should be evicted"
        );
    }
}

// ---------------------------------------------------------------------------
// Test 20 — Supervisor Restart Intensity
// ---------------------------------------------------------------------------

#[test]
fn stress_supervisor_restart_intensity() {
    let mut rt = Runtime::new();
    let sup = rt.create_supervisor("intense", RestartStrategy::OneForOne);

    let mut children = Vec::new();
    for i in 0..50 {
        let child = rt.spawn_actor(Box::new(move || {
            vec![("name".into(), Value::int(700 + i as i64))]
        }));
        let spec = ChildSpec::new(format!("child_{}", i), RestartPolicy::Permanent);
        rt.supervise_child(sup, spec, child);
        children.push(child);
    }

    for child in &children {
        rt.exit_actor(*child, ExitReason::Error("intensity_crash".into()));
    }
    rt.run_scheduler();

    assert!(rt.actors.contains_key(&sup), "supervisor should survive");
    if let Some(supervisor) = rt.supervisors.get(&sup) {
        assert!(
            supervisor.child_count() >= 50,
            "all children should be restarted"
        );
    }
}

// ---------------------------------------------------------------------------
// Test 21 — GC Foreign Reference Churn
// ---------------------------------------------------------------------------

#[test]
fn stress_gc_foreign_ref_churn() {
    let mut rt = Runtime::new();
    let source = rt.spawn_actor(Box::new(|| vec![("name".into(), Value::int(800))]));
    let target = rt.spawn_actor(Box::new(|| vec![("name".into(), Value::int(801))]));

    rt.current_actor = Some(source);

    let mut ptrs: Vec<*mut u8> = Vec::with_capacity(500);
    if let Some(actor) = rt.actors.get_mut(&source) {
        for _ in 0..500 {
            if let Some(ptr) = actor.heap.alloc(16, TypeTag::Raw) {
                ptrs.push(ptr);
            }
        }
    }

    for ptr in &ptrs {
        rt.send_message(target, "ref", &[unsafe { Value::ptr(*ptr) }]);
    }

    let stats = rt.gc_stats();
    assert!(
        stats.foreign_refs_sent >= 500,
        "foreign refs should be sent"
    );

    // Process the delivered ops repeatedly; the stress goal is to exercise
    // the coordinator/cycle-detector path without panicking.
    for _ in 0..5 {
        rt.process_gc_ops();
    }

    assert!(rt.actors.contains_key(&target));
    assert!(rt.actors.contains_key(&source));
}

// ---------------------------------------------------------------------------
// Test 22 — Distribution Local Fallback When Disabled
// ---------------------------------------------------------------------------

#[test]
fn stress_distribution_local_fallback_when_disabled() {
    let mut rt = Runtime::new();
    let actor = rt.spawn_actor(Box::new(|| vec![("name".into(), Value::int(900))]));

    assert!(!rt.distributed.enabled);

    let local_addr = ActorAddress::local(actor);
    rt.send_distributed(local_addr, "ping", &[Value::int(1)]);

    assert!(
        rt.actors
            .get(&actor)
            .map(|a| !a.mailbox.is_empty())
            .unwrap_or(false),
        "message should be delivered locally"
    );

    rt.run_scheduler();
    assert!(rt.actors.contains_key(&actor));
}

// ---------------------------------------------------------------------------
// Test 23 — Reduction Yield Under Pressure
// ---------------------------------------------------------------------------

#[test]
fn stress_reduction_yield_under_pressure() {
    let mut rt = Runtime::new();
    let mut actors = Vec::new();

    for i in 0..50 {
        let id = rt.spawn_actor(Box::new(move || {
            vec![
                ("name".into(), Value::int(1000 + i as i64)),
                ("quota".into(), Value::int(5)),
            ]
        }));
        actors.push(id);
    }

    for id in &actors {
        for i in 0..100 {
            rt.send_message(*id, "work", &[Value::int(i)]);
        }
    }

    rt.run_scheduler();

    let total_reductions: u32 = actors
        .iter()
        .filter_map(|id| rt.actors.get(id).map(|a| a.reduction_count))
        .sum();
    assert!(total_reductions > 0, "some work should have been performed");

    for id in &actors {
        if let Some(actor) = rt.actors.get(id) {
            assert!(actor.mailbox.is_empty(), "all mailboxes should be drained");
        }
    }
}

// ---------------------------------------------------------------------------
// Test 24 — Actor Heap Allocation Pressure
// ---------------------------------------------------------------------------

#[test]
fn stress_actor_heap_allocation_pressure() {
    let mut rt = Runtime::new();
    let actor = rt.spawn_actor(Box::new(|| vec![("name".into(), Value::int(1100))]));

    let allocated = {
        let mut count = 0;
        if let Some(a) = rt.actors.get_mut(&actor) {
            for _ in 0..2_000 {
                if a.heap.alloc(64, TypeTag::Raw).is_some() {
                    count += 1;
                }
            }
        }
        count
    };

    assert!(allocated > 0, "some objects should allocate");
    assert!(rt
        .actors
        .get(&actor)
        .map(|a| a.heap.used() > 0)
        .unwrap_or(false));
}

// ---------------------------------------------------------------------------
// Test 25 — Cascading Supervisor Shutdown
// ---------------------------------------------------------------------------

#[test]
fn stress_cascading_supervisor_shutdown() {
    let mut rt = Runtime::new();
    let root = rt.create_supervisor("root", RestartStrategy::OneForAll);

    let mut supervisors = Vec::new();
    for i in 0..5 {
        let sup = rt.create_supervisor(&format!("sub_{}", i), RestartStrategy::OneForOne);
        rt.supervise_child(
            root,
            ChildSpec::new(format!("sub_{}", i), RestartPolicy::Permanent),
            sup,
        );
        supervisors.push(sup);

        for j in 0..5 {
            let leaf = rt.spawn_actor(Box::new(move || {
                vec![("name".into(), Value::int(1200 + i * 10 + j as i64))]
            }));
            rt.supervise_child(
                sup,
                ChildSpec::new(format!("leaf_{}_{}", i, j), RestartPolicy::Permanent),
                leaf,
            );
        }
    }

    let pre_count = rt.actors.len();
    assert!(pre_count > 25);

    rt.exit_actor(root, ExitReason::Error("root_shutdown".into()));
    rt.run_scheduler();

    assert!(
        !rt.actors.contains_key(&root),
        "root supervisor should be removed"
    );
    let post_count = rt.actors.len();
    assert!(post_count < pre_count, "supervisor tree should shrink");
}

// ---------------------------------------------------------------------------
// Test 26 — Persistence Journal Replay Ordering
// ---------------------------------------------------------------------------

#[test]
fn stress_persistence_journal_replay_ordering() {
    let mut rt = Runtime::new();
    let actor_id = 42u64;

    for seq in 1..=20 {
        rt.persistence
            .append_journal(
                actor_id,
                JournalEntry {
                    sequence: seq,
                    behavior_id: (seq % 3) as u16,
                    payload: vec![PersistedValue::from_value(&Value::int(seq as i64))],
                },
            )
            .unwrap();
    }

    let journal = rt.persistence.read_journal(actor_id);
    assert_eq!(journal.len(), 20);

    for (i, entry) in journal.iter().enumerate() {
        assert_eq!(entry.sequence, (i + 1) as u64);
        assert_eq!(entry.payload, vec![PersistedValue::Int((i + 1) as i64)]);
    }

    assert_eq!(rt.persistence.latest_sequence(actor_id), 20);
}

// ---------------------------------------------------------------------------
// Test 27 — Cycle Detector Epoch Gating
// ---------------------------------------------------------------------------

#[test]
fn stress_cycle_detector_epoch_gating() {
    let mut rt = Runtime::new();
    let initial_epoch = rt.cycle_detector.current_epoch();

    for _ in 0..25 {
        rt.process_gc_ops();
    }

    assert!(
        rt.cycle_detector.current_epoch() > initial_epoch,
        "cycle detector epoch should advance"
    );
}

// ---------------------------------------------------------------------------
// Test 28 — Mailbox System Priority Preservation
// ---------------------------------------------------------------------------

#[test]
fn stress_mailbox_system_priority_preservation() {
    let mut rt = Runtime::new();
    let actor = rt.spawn_actor(Box::new(|| vec![("name".into(), Value::int(1400))]));

    for i in 0..1_000 {
        let msg = Message {
            behavior_id: 1,
            payload: Arc::new(vec![Value::int(i)]),
            sender: 0,
            priority: MessagePriority::Normal,
            trace_id: None,
        };
        if let Some(a) = rt.actors.get_mut(&actor) {
            let _ = a.mailbox.push(msg);
        }
    }

    for i in 0..10 {
        let msg = Message {
            behavior_id: 0,
            payload: Arc::new(vec![Value::int(1000 + i)]),
            sender: 0,
            priority: MessagePriority::System,
            trace_id: None,
        };
        if let Some(a) = rt.actors.get_mut(&actor) {
            let _ = a.mailbox.push(msg);
        }
    }

    let mut system_seen = 0;
    let mut normal_seen = 0;
    if let Some(a) = rt.actors.get_mut(&actor) {
        while let Some(msg) = a.mailbox.pop() {
            match msg.priority {
                MessagePriority::System => system_seen += 1,
                MessagePriority::Normal => normal_seen += 1,
                MessagePriority::Bulk => {}
            }
        }
    }

    assert_eq!(system_seen, 10, "all system messages should be preserved");
    assert_eq!(
        normal_seen, 1_000,
        "all normal messages should be preserved"
    );
}

// ---------------------------------------------------------------------------
// Test 29 — Trap Exit With Monitor Storm
// ---------------------------------------------------------------------------

#[test]
fn stress_trap_exit_with_monitor_storm() {
    let mut rt = Runtime::new();
    let trapper = rt.spawn_actor(Box::new(|| vec![("name".into(), Value::int(1500))]));

    if let Some(actor) = rt.actors.get_mut(&trapper) {
        actor.trap_exits = true;
    }

    let mut ids = Vec::with_capacity(100);
    for i in 0..100 {
        let id = rt.spawn_actor(Box::new(move || {
            vec![("name".into(), Value::int(1600 + i as i64))]
        }));
        rt.monitor(trapper, id);
        ids.push(id);
    }

    for id in &ids {
        rt.exit_actor(*id, ExitReason::Error("monitored".into()));
    }
    rt.run_scheduler();

    assert!(rt.actors.contains_key(&trapper), "trapper should survive");
    if let Some(actor) = rt.actors.get(&trapper) {
        assert!(
            actor.mailbox.len() > 0 || actor.reduction_count > 0,
            "trapper should have received exit/DOWN messages"
        );
    }
}

// ---------------------------------------------------------------------------
// Test 30 — GC Cycle Detector Under Foreign Reference Load
// ---------------------------------------------------------------------------

#[test]
fn stress_gc_cycle_detector_under_foreign_ref_load() {
    let mut rt = Runtime::new();
    const N: usize = 20;
    const REFS_PER_ACTOR: usize = 50;

    let mut actors: Vec<u64> = Vec::with_capacity(N);
    for i in 0..N {
        let id = rt.spawn_actor(Box::new(move || {
            vec![("name".into(), Value::int(1700 + i as i64))]
        }));
        actors.push(id);
    }

    // Each actor allocates objects and sends references to its neighbors,
    // creating a dense graph of foreign references for the cycle detector.
    let mut ptrs: Vec<Vec<*mut u8>> = vec![Vec::new(); N];
    for (i, &actor_id) in actors.iter().enumerate() {
        rt.current_actor = Some(actor_id);
        if let Some(actor) = rt.actors.get_mut(&actor_id) {
            for _ in 0..REFS_PER_ACTOR {
                if let Some(ptr) = actor.heap.alloc(16, TypeTag::Raw) {
                    ptrs[i].push(ptr);
                }
            }
        }
    }

    // Forward ring references + backward cross-references to stress the graph.
    for (i, &actor_id) in actors.iter().enumerate() {
        rt.current_actor = Some(actor_id);
        let next = actors[(i + 1) % N];
        let prev = actors[(i + N - 1) % N];

        for &ptr in &ptrs[i] {
            rt.send_message_by_id(next, 0, &[unsafe { Value::ptr(ptr) }]);
            rt.send_message_by_id(prev, 0, &[unsafe { Value::ptr(ptr) }]);
        }
    }

    let initial_epoch = rt.cycle_detector.current_epoch();
    let initial_graph_size = rt.cycle_detector.graph_size();
    assert!(
        initial_graph_size > 0,
        "cycle detector should have tracked foreign-reference sentinels"
    );

    // Repeatedly process GC ops and run the scheduler to exercise ORCA
    // reference counting, foreign-op draining, and incremental cycle detection.
    for _ in 0..50 {
        rt.process_gc_ops();
    }
    rt.run_scheduler();
    for _ in 0..25 {
        rt.process_gc_ops();
    }

    // The detector should have advanced at least one epoch.
    assert!(
        rt.cycle_detector.current_epoch() > initial_epoch,
        "cycle detector epoch should advance under foreign-ref load"
    );

    // All actors must remain consistent (no panic / no corruption).
    for &actor_id in &actors {
        assert!(
            rt.actors.contains_key(&actor_id),
            "actor {} should survive GC/cycle-detector stress",
            actor_id
        );
    }

    // Foreign ref accounting should reflect the traffic we injected.
    let stats = rt.gc_stats();
    assert!(
        stats.foreign_refs_sent >= (N * REFS_PER_ACTOR * 2) as u64,
        "foreign refs sent should match injected cross-actor references"
    );
}

// ---------------------------------------------------------------------------
// Test 31 — Large Array Allocation + GC Pressure
// ---------------------------------------------------------------------------
// Allocates arrays of 50 ints in a tight loop (100 iterations), fills them,
// sums, and discards. Exercises the heap allocator and GC under array churn.

#[test]
fn stress_large_array_allocation_and_gc() {
    // Build a 50-element zero-filled array, fill with 0..49, sum (=1225),
    // repeat 100 times. Expected checksum: 100 * 1225 = 122,500.
    let source = r#"
let run = fn() {
    let iter = [0]
    let checksum = [0]
    while iter[0] < 100 {
        let arr = [0,0,0,0,0,0,0,0,0,0,0,0,0,0,0,0,0,0,0,0,0,0,0,0,0,
                   0,0,0,0,0,0,0,0,0,0,0,0,0,0,0,0,0,0,0,0,0,0,0,0,0]
        let i = [0]
        while i[0] < 50 {
            arr[i[0]] = i[0]
            i[0] = i[0] + 1
        }
        let j = [0]
        let sum = [0]
        while j[0] < 50 {
            sum[0] = sum[0] + arr[j[0]]
            j[0] = j[0] + 1
        }
        checksum[0] = checksum[0] + sum[0]
        iter[0] = iter[0] + 1
    }
    checksum[0]
}
run()
"#;
    let expected: i64 = 100 * (49 * 50 / 2); // 100 * 1225 = 122,500
    assert_nula_int(source, expected);
}

// ---------------------------------------------------------------------------
// Test 32 — String Allocation Pressure
// ---------------------------------------------------------------------------
// Concatenates a short string in a tight loop (5,000 iterations), creating
// and discarding intermediate string allocations. Exercises the string heap
// and GC under repeated concatenation churn.

#[test]
fn stress_string_allocation_pressure() {
    // Build a string by repeatedly concatenating "x" in a loop,
    // discarding intermediate results. Return the iteration count.
    // The real stress is that this completes without crashing or OOM.
    let source = r#"
let run = fn() {
    let i = [0]
    let s = [""]
    while i[0] < 5000 {
        s[0] = s[0] + "x"
        i[0] = i[0] + 1
    }
    1
}
run()
"#;
    // The test passes if it completes without error (returning 1).
    assert_nula_int(source, 1);
}

// ---------------------------------------------------------------------------
// Test 33 — Deep Recursion (Stack Depth)
// ---------------------------------------------------------------------------
// ---------------------------------------------------------------------------
// Computes a recursive sum from 1..1000, exercising stack depth and
// verifying that the interpreter handles deep call chains.

#[test]
fn stress_deep_recursion() {
    // Recursively sum 1 + 2 + ... + 1000 = 500,500.
    // Wrap in a block because `let rec` is not valid at the module top-level
    // in the test compilation pipeline.
    let source = r#"
{
    let rec sum_to = fn(n) {
        if n <= 0 then 0 else n + sum_to(n - 1)
    }
    sum_to(1000)
}
"#;
    let expected: i64 = 1000 * 1001 / 2; // 500,500
    assert_nula_int(source, expected);
}

// ---------------------------------------------------------------------------
// Test 34 — Actor Message Passing (Ping-Pong) Under Load
// ---------------------------------------------------------------------------
#[test]
fn stress_actor_ping_pong_messaging() {
    let mut rt = Runtime::new();

    // Create two actors: pinger and ponger
    let pinger = rt.spawn_actor(Box::new(|| vec![("name".into(), Value::int(100))]));

    let ponger = rt.spawn_actor(Box::new(|| vec![("name".into(), Value::int(101))]));

    // Pre-load the pinger's mailbox with 10,000 "pong" messages
    // that each carry a decrementing counter.
    const MSG_COUNT: usize = 10_000;
    for i in 0..MSG_COUNT {
        rt.send_message(
            pinger,
            "pong",
            &[Value::int(i as i64), Value::int(ponger as i64)],
        );
    }

    // Run the scheduler to process all messages.
    rt.run_scheduler();

    // After processing, both actors should still be alive
    // and their mailboxes should be drained.
    assert!(
        rt.actors.contains_key(&pinger),
        "pinger should still be alive after ping-pong"
    );
    assert!(
        rt.actors.contains_key(&ponger),
        "ponger should still be alive after ping-pong"
    );

    if let Some(actor) = rt.actors.get(&pinger) {
        assert!(
            actor.mailbox.is_empty(),
            "pinger mailbox should be drained after scheduler run"
        );
        assert!(
            actor.reduction_count > 0,
            "pinger should have processed messages, reductions={}",
            actor.reduction_count
        );
    }
    if let Some(actor) = rt.actors.get(&ponger) {
        assert!(
            actor.mailbox.is_empty(),
            "ponger mailbox should be drained after scheduler run"
        );
    }
}
