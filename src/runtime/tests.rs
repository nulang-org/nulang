//! Runtime integration tests.
//!
//! 84 tests total (see AGENTS.md "Testing & QA" for the suite-wide counts).
//! Full history in local commit 1c2cde9.

use super::*;
use crate::bytecode::{ActorMeta, CodeModule, Constant};
use crate::runtime::gc::OrcaGc;
use crate::runtime::heap::{ActorHeap, TypeTag};
use crate::vm::{Frame, Value};
use std::collections::HashSet;
use std::sync::Arc;
use std::time::{Duration, Instant};

// ========================================================================
// Core Runtime Tests
// ========================================================================

#[test]
fn test_spawn_send_step_sequence() {
    let mut rt = Runtime::new();
    let actor_id = rt.spawn_actor(Box::new(|| vec![("count".to_string(), Value::int(0))]));
    assert!(rt.actors.contains_key(&actor_id));
    {
        let actor = rt.actors.get_mut(&actor_id).unwrap();
        actor.register_behavior("inc", |actor, args| {
            if let Some(n) = actor.get_state_field("counter").and_then(|v| v.as_int()) {
                if let Some(incr) = args.get(0).and_then(|v| v.as_int()) {
                    actor.set_state_field("counter", Value::int(n + incr));
                }
            }
        });
    }
    rt.send_message(actor_id, "inc", &[Value::int(1)]);
    rt.step_actor(actor_id);
    // Message processed
}

#[test]
fn test_mailbox_push_pop() {
    let mut mb = Mailbox::new(4);
    let msg = Message {
        behavior_id: 0,
        payload: Arc::new(vec![Value::int(42)]),
        sender: 1,
        priority: MessagePriority::Normal,
        trace_id: None,
    };
    assert!(mb.push(msg.clone()).is_ok());
    assert_eq!(mb.len(), 1);
    let popped = mb.pop().unwrap();
    assert_eq!(popped.payload[0].as_int(), Some(42));
    assert!(mb.is_empty());
}

#[test]
fn test_send_carries_current_trace_span() {
    let mut rt = Runtime::new();
    let b = rt.spawn_actor(Box::new(|| vec![]));
    let root = TraceContext::root();
    rt.current_trace = Some(root);
    rt.send_message_by_id(b, 0, &[]);
    let actor = rt.actors.get_mut(&b).unwrap();
    let msg = actor.receive().expect("message delivered");
    let tp = msg
        .trace_id
        .expect("outgoing message carries a traceparent");
    let parsed = TraceContext::from_traceparent(&tp).expect("valid traceparent");
    // `traceparent` carries the current span (trace-id + span-id), so the
    // outgoing message exposes the handler's span for the receiver to child.
    assert_eq!(parsed.trace_id(), root.trace_id());
    assert_eq!(parsed.span_id(), root.span_id());
}

#[test]
fn test_delivery_establishes_child_context_and_inherits() {
    let mut rt = Runtime::new();
    let a = rt.spawn_actor(Box::new(|| vec![]));
    // Deliver a message carrying a known W3C traceparent (the W3C spec example).
    let incoming = "00-4bf92f3577b34da6a3ce929d0e0e4736-00f067aa0ba902b7-01";
    {
        let actor = rt.actors.get_mut(&a).unwrap();
        actor
            .mailbox
            .push(Message {
                behavior_id: 0,
                payload: Arc::new(vec![]),
                sender: 0,
                priority: MessagePriority::Normal,
                trace_id: Some(incoming.to_string()),
            })
            .unwrap();
    }
    rt.step_actor(a);
    let ctx = rt
        .current_trace
        .expect("delivery establishes a trace context");
    assert_eq!(ctx.trace_id(), 0x4bf9_2f35_77b3_4da6_a3ce_929d_0e0e_4736);
    assert_eq!(ctx.parent_span_id(), 0x00f0_67aa_0ba9_02b7);

    // A send performed after delivery inherits the same trace.
    let b = rt.spawn_actor(Box::new(|| vec![]));
    rt.send_message_by_id(b, 0, &[]);
    let msg = rt.actors.get_mut(&b).unwrap().receive().expect("delivered");
    let child = TraceContext::from_traceparent(&msg.trace_id.expect("stamped")).unwrap();
    // The send carries `ctx`'s span (traceparent has no parent field); a
    // receiver would child off it, continuing the same trace.
    assert_eq!(child.trace_id(), ctx.trace_id());
    assert_eq!(child.span_id(), ctx.span_id());
}

#[test]
fn test_metrics_snapshot_topology_and_crdt() {
    let mut rt = Runtime::new();

    // Supervision tree: root supervisor with one child.
    let sup_id = rt.create_supervisor("root", RestartStrategy::OneForOne);
    let child_id = rt.spawn_actor(Box::new(|| vec![]));
    let spec = ChildSpec::new("child1", RestartPolicy::Permanent);
    rt.supervise_child(sup_id, spec, child_id);

    // CRDT replica with a change that hasn't been synced.
    rt.crdt_manager = Some(crate::runtime::crdt_manager::CrdtManager::new(42));
    let (_, mut counter) = rt.crdt_manager.as_mut().unwrap().create_gcounter();
    counter.increment();

    let snap = rt.metrics_snapshot();

    // Supervision topology.
    assert_eq!(snap.supervisors.len(), 1);
    let sup = &snap.supervisors[0];
    assert_eq!(sup.id, sup_id);
    assert_eq!(sup.name, "root");
    assert_eq!(sup.strategy, "OneForOne");
    assert_eq!(sup.parent, None);
    assert_eq!(sup.children.len(), 1);
    assert_eq!(sup.children[0].actor_id, child_id);
    assert_eq!(sup.children[0].spec_id, "child1");

    // CRDT replication state.
    assert_eq!(snap.crdt.node_id, 42);
    assert_eq!(snap.crdt.entries, 1);
    assert_eq!(snap.crdt.ops_synced, 0);
    assert!(
        snap.crdt.unsynced_deltas >= 1,
        "created-but-unsynced entry must count as an unsynced delta, got {}",
        snap.crdt.unsynced_deltas
    );
}

#[test]
fn test_render_topology_nested_supervisors() {
    let mut rt = Runtime::new();
    // top -> mid -> leaf actor
    let top_id = rt.create_supervisor("top", RestartStrategy::OneForAll);
    let mid_id = rt.create_supervisor("mid", RestartStrategy::OneForOne);
    let leaf_id = rt.spawn_actor(Box::new(|| vec![]));
    rt.supervise_child(
        top_id,
        ChildSpec::new("m", RestartPolicy::Permanent),
        mid_id,
    );
    rt.supervise_child(
        mid_id,
        ChildSpec::new("l", RestartPolicy::Transient),
        leaf_id,
    );

    let text = rt.render_topology();
    let top_pos = text
        .find("supervisor top [OneForAll]")
        .expect("top rendered");
    let mid_pos = text
        .find("supervisor mid [OneForOne]")
        .expect("mid rendered");
    assert!(
        text.contains(&format!("actor {leaf_id} (l)")),
        "leaf rendered: {text}"
    );
    assert!(mid_pos > top_pos, "mid must nest under top:\n{text}");
}

#[test]
fn test_scheduler_enqueue_steal() {
    let sched = Scheduler::new(4);
    assert!(sched.steal_one().is_none());
    sched.enqueue(100);
    sched.enqueue(200);
    // Global injector is FIFO: 100 was enqueued first, so it's stolen first
    assert_eq!(sched.steal_one(), Some(100));
    assert_eq!(sched.steal_one(), Some(200));
    assert!(sched.steal_one().is_none());
}

#[test]
fn test_actor_register_behavior() {
    let mut actor = Actor::new(1, "test_actor", 0);
    actor.register_behavior("hello", |_actor, _args| {});
    assert_eq!(actor.behavior_table.len(), 1);
    assert_eq!(actor.behavior_table[0].name, "hello");
}

#[test]
fn test_run_scheduler_processes_all_actors() {
    let mut rt = Runtime::new();
    let a1 = rt.spawn_actor(Box::new(|| vec![("counter".to_string(), Value::int(0))]));
    let a2 = rt.spawn_actor(Box::new(|| vec![("counter".to_string(), Value::int(0))]));
    rt.send_message(a1, "add", &[Value::int(10)]);
    rt.send_message(a2, "add", &[Value::int(20)]);
    rt.run_scheduler();
}

// ========================================================================
// Actor Priority Tests
// ========================================================================

#[test]
fn test_actor_priority_default_is_normal() {
    let actor = Actor::new(1, "test_actor", 0);
    assert_eq!(actor.priority, ActorPriority::Normal);
    assert_eq!(ActorPriority::default(), ActorPriority::Normal);
}

#[test]
fn test_scheduler_priority_dequeue_order() {
    // Strict per-level preference: every High entry drains before any
    // Normal, every Normal before any Low; FIFO within a level.
    let sched = Scheduler::new(4);
    sched.enqueue_with_priority(1, ActorPriority::Normal);
    sched.enqueue_with_priority(2, ActorPriority::Low);
    sched.enqueue_with_priority(3, ActorPriority::High);
    sched.enqueue_with_priority(4, ActorPriority::Normal);
    sched.enqueue_with_priority(5, ActorPriority::High);
    sched.enqueue_with_priority(6, ActorPriority::Low);
    assert_eq!(sched.steal_one(), Some(3));
    assert_eq!(sched.steal_one(), Some(5));
    assert_eq!(sched.steal_one(), Some(1));
    assert_eq!(sched.steal_one(), Some(4));
    assert_eq!(sched.steal_one(), Some(2));
    assert_eq!(sched.steal_one(), Some(6));
    assert!(sched.steal_one().is_none());
}

#[test]
fn test_scheduler_enqueue_defaults_to_normal() {
    // The plain `enqueue` entry point lands in the Normal level.
    let sched = Scheduler::new(2);
    sched.enqueue(1); // Normal
    sched.enqueue_with_priority(2, ActorPriority::High);
    sched.enqueue_with_priority(3, ActorPriority::Low);
    assert_eq!(sched.steal_one(), Some(2));
    assert_eq!(sched.steal_one(), Some(1));
    assert_eq!(sched.steal_one(), Some(3));
}

#[test]
fn test_actor_set_priority_effect_maps_levels() {
    let mut rt = Runtime::new();
    let a = rt.spawn_actor(Box::new(|| vec![]));
    let set = |rt: &mut Runtime, who: Option<u64>, level: i64| {
        rt.perform_actor_builtin(who, Some("set_priority"), &[], &[Value::int(level)])
    };
    assert_eq!(set(&mut rt, Some(a), 0), Some(Value::nil()));
    assert_eq!(rt.actors.get(&a).unwrap().priority, ActorPriority::High);
    assert_eq!(set(&mut rt, Some(a), 2), Some(Value::nil()));
    assert_eq!(rt.actors.get(&a).unwrap().priority, ActorPriority::Low);
    assert_eq!(set(&mut rt, Some(a), 1), Some(Value::nil()));
    assert_eq!(rt.actors.get(&a).unwrap().priority, ActorPriority::Normal);
    // Out-of-range levels fall back to Normal.
    assert_eq!(set(&mut rt, Some(a), 7), Some(Value::nil()));
    assert_eq!(rt.actors.get(&a).unwrap().priority, ActorPriority::Normal);
    // Outside an actor context the effect is a nil no-op.
    assert_eq!(set(&mut rt, None, 0), Some(Value::nil()));
}

#[test]
fn test_actor_set_priority_changes_scheduling() {
    // A High-priority actor is dequeued before a Normal one even when the
    // Normal actor's message was sent first.
    let mut rt = Runtime::new();
    let a = rt.spawn_actor(Box::new(|| vec![]));
    let b = rt.spawn_actor(Box::new(|| vec![]));
    // Drain the spawn-time queue entries (both enqueued at Normal).
    assert_eq!(rt.scheduler.dequeue(), Some(a));
    assert_eq!(rt.scheduler.dequeue(), Some(b));
    // Boost b via the builtin-effect path, then send to a before b.
    assert_eq!(
        rt.perform_actor_builtin(Some(b), Some("set_priority"), &[], &[Value::int(0)]),
        Some(Value::nil())
    );
    rt.send_message(a, "noop", &[]);
    rt.send_message(b, "noop", &[]);
    assert_eq!(rt.scheduler.dequeue(), Some(b));
    assert_eq!(rt.scheduler.dequeue(), Some(a));
}

// ========================================================================
// Supervisor Tests
// ========================================================================

#[test]
fn test_one_for_one_restart() {
    let mut rt = Runtime::new();
    let sup_id = rt.create_supervisor("test_sup", RestartStrategy::OneForOne);
    let child_id = rt.spawn_actor(Box::new(|| vec![("x".to_string(), Value::int(0))]));
    let spec = ChildSpec::new("child1", RestartPolicy::Permanent);
    rt.supervise_child(sup_id, spec, child_id);
    assert_eq!(rt.supervisors[&sup_id].child_count(), 1);
    rt.exit_actor(child_id, ExitReason::Error("crash".to_string()));
    assert!(!rt.actors.contains_key(&child_id));
    assert_eq!(rt.supervisors[&sup_id].child_count(), 1);
}

#[test]
fn test_supervisor_restart_rate_limiting() {
    let mut rt = Runtime::new();
    let sup_id = rt.create_supervisor("rate_sup", RestartStrategy::OneForOne);
    let child_id = rt.spawn_actor(Box::new(|| vec![]));
    let spec = ChildSpec::new("fragile", RestartPolicy::Permanent).with_limits(2, 60);
    rt.supervise_child(sup_id, spec, child_id);

    // Crash 1: child should be restarted (restart #1)
    rt.exit_actor(child_id, ExitReason::Error("crash1".to_string()));
    let child_id_2 = rt.supervisors[&sup_id].children[0].1;
    assert_eq!(rt.supervisors[&sup_id].restart_count(child_id_2), 1);

    // Crash 2: child should be restarted again (restart #2)
    rt.exit_actor(child_id_2, ExitReason::Error("crash2".to_string()));
    let child_id_3 = rt.supervisors[&sup_id].children[0].1;
    assert_eq!(rt.supervisors[&sup_id].restart_count(child_id_3), 2);

    // Crash 3: max_restarts=2 exceeded → supervisor shuts down
    rt.exit_actor(child_id_3, ExitReason::Error("crash3".to_string()));
    assert!(
        !rt.supervisors.contains_key(&sup_id),
        "supervisor should shut down after max restarts"
    );
}

#[test]
fn test_supervisor_escalate_to_parent() {
    let mut rt = Runtime::new();
    let parent_sup = rt.create_supervisor("parent", RestartStrategy::OneForOne);
    let child_sup = rt.create_supervisor("child", RestartStrategy::OneForOne);

    rt.supervisors.get_mut(&child_sup).unwrap().parent = Some(parent_sup);
    let grandchild = rt.spawn_actor(Box::new(|| vec![]));
    let spec = ChildSpec::new("gc", RestartPolicy::Permanent).with_limits(1, 60);
    rt.supervise_child(child_sup, spec, grandchild);

    rt.exit_actor(grandchild, ExitReason::Error("boom".to_string()));
    assert!(
        rt.actors.contains_key(&child_sup),
        "child supervisor should still exist after one restart"
    );

    let gc2 = rt.supervisors[&child_sup].children[0].1;
    rt.exit_actor(gc2, ExitReason::Error("boom2".to_string()));
}

/// Regression (Phase 5 deliverable 9): restarting a supervisor actor must
/// recreate its `Supervisor` struct under the new actor id — a supervised
/// supervisor that loses its struct stops supervising its own children.
#[test]
fn test_supervised_supervisor_keeps_supervising_after_restart() {
    let mut rt = Runtime::new();
    let parent = rt.create_supervisor("parent", RestartStrategy::OneForOne);
    let child_sup = rt.create_supervisor("child", RestartStrategy::OneForOne);
    rt.supervise_child(
        parent,
        ChildSpec::new("child_sup", RestartPolicy::Permanent),
        child_sup,
    );
    let grandchild = rt.spawn_actor(Box::new(|| vec![]));
    rt.supervise_child(
        child_sup,
        ChildSpec::new("gc", RestartPolicy::Permanent),
        grandchild,
    );
    rt.supervisors.get_mut(&child_sup).unwrap().parent = Some(parent);

    // Crash the child supervisor's actor: the parent restarts it under a
    // fresh actor id.
    rt.exit_actor(child_sup, ExitReason::Error("sup crashed".to_string()));
    let new_sup_id = rt.supervisors[&parent].children[0].1;
    assert_ne!(new_sup_id, child_sup, "supervisor actor must be rebuilt");

    // The Supervisor struct must follow the actor to its new id, still
    // supervising the grandchild.
    assert!(
        rt.supervisors.contains_key(&new_sup_id),
        "restarted supervisor must have a live Supervisor struct"
    );
    assert!(
        !rt.supervisors.contains_key(&child_sup),
        "the old supervisor struct must not linger under a dead actor id"
    );
    let gc_id = rt.supervisors[&new_sup_id].children[0].1;
    assert_eq!(
        rt.actors.get(&gc_id).unwrap().parent,
        Some(new_sup_id),
        "grandchild must be re-pointed at the new supervisor id"
    );

    // Supervision must actually work through the recreated struct: a
    // grandchild crash restarts it.
    rt.exit_actor(gc_id, ExitReason::Error("boom".to_string()));
    let new_gc = rt.supervisors[&new_sup_id].children[0].1;
    assert_ne!(
        new_gc, gc_id,
        "grandchild must be restarted by the recreated supervisor"
    );
}

/// Regression (Phase 5 deliverable 10): a OneForAll restart must respect
/// each sibling's own restart intensity. A sibling whose MaxR is exhausted
/// must be dropped, not rebuilt — otherwise the group can restart-loop
/// forever even though every child's limits are individually respected.
#[test]
fn test_one_for_all_respects_sibling_rate_limit() {
    let mut rt = Runtime::new();
    let sup_id = rt.create_supervisor("sup", RestartStrategy::OneForAll);
    let trigger = rt.spawn_actor(Box::new(|| vec![]));
    rt.supervise_child(
        sup_id,
        ChildSpec::new("trigger", RestartPolicy::Permanent),
        trigger,
    );
    let fragile = rt.spawn_actor(Box::new(|| vec![]));
    rt.supervise_child(
        sup_id,
        ChildSpec::new("fragile", RestartPolicy::Permanent).with_limits(1, 60),
        fragile,
    );
    let sibling = rt.spawn_actor(Box::new(|| vec![]));
    rt.supervise_child(
        sup_id,
        ChildSpec::new("sibling", RestartPolicy::Permanent),
        sibling,
    );

    // Crash 1: the OneForAll cascade rebuilds every child, recording
    // fragile's first (and only permitted) restart.
    rt.exit_actor(trigger, ExitReason::Error("crash1".to_string()));
    assert_eq!(rt.supervisors[&sup_id].child_count(), 3);

    // Crash 2: the cascade must NOT rebuild fragile again — its MaxR of 1
    // is exhausted. It is stopped and dropped from supervision instead.
    let trigger2 = rt.supervisors[&sup_id]
        .children
        .iter()
        .find(|(s, _)| s.id == "trigger")
        .unwrap()
        .1;
    rt.exit_actor(trigger2, ExitReason::Error("crash2".to_string()));
    assert_eq!(
        rt.supervisors[&sup_id].child_count(),
        2,
        "the rate-limited sibling must be dropped, not rebuilt"
    );
    assert!(
        rt.supervisors[&sup_id]
            .children
            .iter()
            .all(|(s, _)| s.id != "fragile"),
        "the rate-limited sibling must be gone from supervision"
    );
}

/// Regression (Phase 5 deliverable 10, RestForOne variant): same per-sibling
/// rate-limit discipline in the restart-from cascade.
#[test]
fn test_rest_for_one_respects_sibling_rate_limit() {
    let mut rt = Runtime::new();
    let sup_id = rt.create_supervisor("sup", RestartStrategy::RestForOne);
    let trigger = rt.spawn_actor(Box::new(|| vec![]));
    rt.supervise_child(
        sup_id,
        ChildSpec::new("trigger", RestartPolicy::Permanent),
        trigger,
    );
    let fragile = rt.spawn_actor(Box::new(|| vec![]));
    rt.supervise_child(
        sup_id,
        ChildSpec::new("fragile", RestartPolicy::Permanent).with_limits(1, 60),
        fragile,
    );
    let sibling = rt.spawn_actor(Box::new(|| vec![]));
    rt.supervise_child(
        sup_id,
        ChildSpec::new("sibling", RestartPolicy::Permanent),
        sibling,
    );

    // Crash fragile: RestForOne restarts fragile and everything after it,
    // recording fragile's only permitted restart.
    rt.exit_actor(fragile, ExitReason::Error("crash1".to_string()));
    assert_eq!(rt.supervisors[&sup_id].child_count(), 3);

    // Crash trigger: the cascade (trigger, fragile, sibling) must not
    // rebuild fragile again.
    let trigger2 = rt.supervisors[&sup_id]
        .children
        .iter()
        .find(|(s, _)| s.id == "trigger")
        .unwrap()
        .1;
    rt.exit_actor(trigger2, ExitReason::Error("crash2".to_string()));
    assert_eq!(
        rt.supervisors[&sup_id].child_count(),
        2,
        "the rate-limited sibling must be dropped, not rebuilt"
    );
    assert!(
        rt.supervisors[&sup_id]
            .children
            .iter()
            .all(|(s, _)| s.id != "fragile"),
        "the rate-limited sibling must be gone from supervision"
    );
}

#[test]
fn test_temporary_child_not_restarted() {
    let mut rt = Runtime::new();
    let sup_id = rt.create_supervisor("sup", RestartStrategy::OneForOne);
    let child_id = rt.spawn_actor(Box::new(|| vec![]));
    let spec = ChildSpec::new("temp_child", RestartPolicy::Temporary);
    rt.supervise_child(sup_id, spec, child_id);
    rt.exit_actor(child_id, ExitReason::Error("boom".to_string()));
    assert_eq!(rt.supervisors[&sup_id].child_count(), 0);
}

/// Regression test: a restarted child must be rebuilt with its behavior
/// table and initial state, not as a bare actor that silently drops every
/// message it receives.
#[test]
fn test_restarted_child_restores_behavior_and_state() {
    let mut rt = Runtime::new();
    let sup_id = rt.create_supervisor("test_sup", RestartStrategy::OneForOne);
    let child_id = rt.spawn_actor(Box::new(|| vec![("count".to_string(), Value::int(0))]));
    {
        let actor = rt.actors.get_mut(&child_id).unwrap();
        actor.register_behavior("inc", |actor, args| {
            let n = actor
                .get_state_field("count")
                .and_then(|v| v.as_int())
                .unwrap_or(0);
            let by = args.get(0).and_then(|v| v.as_int()).unwrap_or(1);
            actor.set_state_field("count", Value::int(n + by));
        });
    }
    let spec = ChildSpec::new("child1", RestartPolicy::Permanent);
    rt.supervise_child(sup_id, spec, child_id);

    rt.exit_actor(child_id, ExitReason::Error("crash".to_string()));
    let new_id = rt.supervisors[&sup_id].children[0].1;
    assert_ne!(new_id, child_id, "restart should create a fresh actor");

    // The restarted child must handle messages (before the fix it was a
    // bare actor that silently dropped them).
    rt.send_message(new_id, "inc", &[Value::int(5)]);
    rt.step_actor(new_id);
    let count = rt.actors.get(&new_id).unwrap().get_state_field("count");
    assert_eq!(count, Some(Value::int(5)));
}

/// Supervisor restart of a persistent child must hydrate state from the
/// persistence store, not from the captured RestartTemplate (which holds
/// the *original* state from registration time).
#[test]
fn test_supervisor_restart_hydrates_from_persistence() {
    let mut rt = Runtime::new();
    rt.persistence = Box::new(MemoryStore::new());
    let sup_id = rt.create_supervisor("sup", RestartStrategy::OneForOne);
    let mut models = HashMap::new();
    models.insert("count".to_string(), StateModel::Durable);
    let child_id = rt.spawn_persistent_actor(
        Box::new(|| vec![("count".to_string(), Value::int(0))]),
        models,
    );
    {
        let actor = rt.actors.get_mut(&child_id).unwrap();
        actor.register_behavior("inc", |actor, args| {
            let n = actor
                .get_state_field("count")
                .and_then(|v| v.as_int())
                .unwrap_or(0);
            let by = args.get(0).and_then(|v| v.as_int()).unwrap_or(1);
            actor.set_state_field("count", Value::int(n + by));
        });
    }

    for _ in 0..3 {
        rt.send_message(child_id, "inc", &[Value::int(1)]);
        rt.step_actor(child_id);
    }
    assert_eq!(
        rt.actors.get(&child_id).unwrap().get_state_field("count"),
        Some(Value::int(3)),
        "count should be 3 before crash"
    );
    assert!(
        rt.persistence.load_snapshot(child_id).is_some(),
        "snapshot should exist before crash"
    );

    let spec = ChildSpec::new("counter", RestartPolicy::Permanent);
    rt.supervise_child(sup_id, spec, child_id);
    rt.exit_actor(child_id, ExitReason::Error("simulated crash".to_string()));

    let new_id = rt.supervisors[&sup_id].children[0].1;
    assert_ne!(new_id, child_id, "restart should create a fresh actor");

    let count = rt.actors.get(&new_id).unwrap().get_state_field("count");
    assert_eq!(
        count,
        Some(Value::int(3)),
        "restarted actor must hydrate count=3 from persistence, not template count=0"
    );

    assert!(
        rt.persistence.load_snapshot(new_id).is_some(),
        "snapshot must be re-keyed under new actor id"
    );
    assert!(
        rt.persistence.load_snapshot(child_id).is_none(),
        "old snapshot must be cleared after re-keying"
    );
}

/// Regression test: a restarted bytecode child must keep its bytecode
/// module, behavior offsets, and captured initial state so it still
/// resolves and runs its bytecode behaviors after a restart.
#[test]
fn test_restarted_bytecode_child_handles_messages() {
    use crate::bytecode::{BehaviorTableEntry, CodeModule, Constant, Instruction, OpCode};

    let mut rt = Runtime::new();
    let sup_id = rt.create_supervisor("byte_sup", RestartStrategy::OneForOne);

    // Behavior "Counter.inc": count += 1, returning the new count.
    let mut module = CodeModule::new("test");
    let field_idx = module.add_constant(Constant::String("count".to_string()));
    let one_idx = module.add_constant(Constant::Int(1));
    module.add_behavior(BehaviorTableEntry {
        name: "Counter.inc".to_string(),
        param_count: 0,
        code_offset: 0,
        local_count: 4,
        effect_mask: 0,
        compensate_offset: None,
        content_hash: None,
        source_location: None,
        parallel_branches: None,
    });
    module.emit(Instruction::new3(
        OpCode::StateGet,
        ((field_idx >> 8) & 0xFF) as u8,
        (field_idx & 0xFF) as u8,
        1,
    ));
    module.emit(Instruction::new3(
        OpCode::ConstU,
        ((one_idx >> 8) & 0xFF) as u8,
        (one_idx & 0xFF) as u8,
        2,
    ));
    module.emit(Instruction::new3(OpCode::IAdd, 1, 2, 3));
    module.emit(Instruction::new3(OpCode::StateSet, 0, 0, 3));
    module.emit(Instruction::new1(OpCode::RetVal, 3));

    let child_id = rt.spawn_actor(Box::new(|| vec![("count".to_string(), Value::int(0))]));
    {
        let actor = rt.actors.get_mut(&child_id).unwrap();
        actor.bytecode_module = Some(module.clone());
        actor.bytecode_offsets = vec![0];
        actor.compensation_offsets = vec![None];
    }
    rt.register_recovery_module(child_id, module, vec![0], vec![None]);
    let spec = ChildSpec::new("counter", RestartPolicy::Permanent);
    rt.supervise_child(sup_id, spec, child_id);

    // Sanity: the behavior works before the crash.
    let before = rt.ask_actor_sync(child_id, 0, &[]).unwrap();
    assert_eq!(before, Value::int(1));

    rt.exit_actor(child_id, ExitReason::Error("crash".to_string()));
    let new_id = rt.supervisors[&sup_id].children[0].1;
    assert_ne!(new_id, child_id);

    // After restart the child must still resolve and run its bytecode
    // behavior (before the fix the bare actor answered every ask with nil).
    assert_eq!(rt.behavior_id_for(new_id, "inc"), Some(0));
    let after = rt.ask_actor_sync(new_id, 0, &[]).unwrap();
    assert_eq!(
        after,
        Value::int(1),
        "restarted child must restart from its captured initial state"
    );
    // And the module was re-registered for recovery after a runtime restart.
    assert!(rt.recovery_modules.contains_key(&new_id));
}

/// Regression test: OneForAll mass restart removes the LIVING sibling
/// children through the full exit protocol — registry names are
/// unregistered and monitors receive a DOWN message.
#[test]
fn test_restart_all_unregisters_names_and_notifies_monitors() {
    let mut rt = Runtime::new();
    let sup_id = rt.create_supervisor("all_sup", RestartStrategy::OneForAll);
    let trigger = rt.spawn_actor(Box::new(|| vec![]));
    let sibling = rt.spawn_actor(Box::new(|| vec![]));
    rt.supervise_child(
        sup_id,
        ChildSpec::new("trigger", RestartPolicy::Permanent),
        trigger,
    );
    rt.supervise_child(
        sup_id,
        ChildSpec::new("sibling", RestartPolicy::Permanent),
        sibling,
    );
    rt.registry.register("sibling_name", sibling).unwrap();
    let watcher = rt.spawn_actor(Box::new(|| vec![]));
    rt.monitor(watcher, sibling);

    rt.exit_actor(trigger, ExitReason::Error("crash".to_string()));

    assert!(
        !rt.actors.contains_key(&sibling),
        "living sibling must be replaced on a OneForAll restart"
    );
    assert_eq!(
        rt.registry.whereis("sibling_name"),
        None,
        "removed child's registered name must not linger"
    );
    let down = rt
        .actors
        .get_mut(&watcher)
        .unwrap()
        .mailbox
        .pop()
        .expect("monitor of the removed sibling must receive a DOWN message");
    assert_eq!(down.payload[0].as_int(), Some(sibling as i64));
    assert_eq!(rt.supervisors[&sup_id].child_count(), 2);
}

/// Regression test: when a supervisor shuts down (restart intensity
/// exceeded), its remaining living children are removed through the exit
/// protocol too — not via a raw map removal.
#[test]
fn test_supervisor_shutdown_cleans_up_children() {
    let mut rt = Runtime::new();
    let sup_id = rt.create_supervisor("rate_sup", RestartStrategy::OneForOne);
    let fragile = rt.spawn_actor(Box::new(|| vec![]));
    rt.supervise_child(
        sup_id,
        ChildSpec::new("fragile", RestartPolicy::Permanent).with_limits(1, 60),
        fragile,
    );
    let sibling = rt.spawn_actor(Box::new(|| vec![]));
    rt.supervise_child(
        sup_id,
        ChildSpec::new("sibling", RestartPolicy::Permanent),
        sibling,
    );
    rt.registry.register("sibling_name", sibling).unwrap();
    let watcher = rt.spawn_actor(Box::new(|| vec![]));
    rt.monitor(watcher, sibling);

    // Crash 1 restarts the fragile child (within limits); crash 2 exceeds
    // the intensity and shuts the supervisor down, which must remove the
    // living sibling through the exit protocol.
    rt.exit_actor(fragile, ExitReason::Error("crash1".to_string()));
    let fragile2 = rt.supervisors[&sup_id]
        .children
        .iter()
        .find(|(s, _)| s.id == "fragile")
        .unwrap()
        .1;
    rt.exit_actor(fragile2, ExitReason::Error("crash2".to_string()));

    assert!(!rt.supervisors.contains_key(&sup_id));
    assert!(!rt.actors.contains_key(&sibling));
    assert_eq!(
        rt.registry.whereis("sibling_name"),
        None,
        "shut-down supervisor must unregister its children's names"
    );
    let down = rt.actors.get_mut(&watcher).unwrap().mailbox.pop();
    assert!(
        down.is_some(),
        "monitor of a child removed by supervisor shutdown must receive DOWN"
    );
}

/// Regression test: a supervised child that exits with an outstanding
/// foreign reference must have its heap retired (not dropped wholesale),
/// exactly like an unsupervised exit via `remove_actor_reaping`.
#[test]
fn test_supervised_child_restart_retires_heap_with_foreign_refs() {
    let mut rt = Runtime::new();
    let sup_id = rt.create_supervisor("reap_sup", RestartStrategy::OneForOne);
    let a = rt.spawn_actor(Box::new(|| vec![]));
    rt.supervise_child(sup_id, ChildSpec::new("a", RestartPolicy::Permanent), a);
    let b = rt.spawn_actor(Box::new(|| vec![]));
    rt.current_actor = Some(a);

    let ptr = rt
        .actors
        .get_mut(&a)
        .unwrap()
        .heap
        .alloc(16, TypeTag::Raw)
        .unwrap();
    let v = Value::ptr(ptr);
    rt.send_message_by_id(b, 0, &[v]);

    // A crashes with the in-flight foreign ref still pending.
    rt.exit_actor(a, ExitReason::Error("crash".to_string()));
    rt.current_actor = None;
    assert!(!rt.actors.contains_key(&a));
    assert_eq!(
        rt.retired_heaps.len(),
        1,
        "supervised child's heap must be retired while foreign refs are outstanding"
    );
    let new_id = rt.supervisors[&sup_id].children[0].1;
    assert_ne!(new_id, a, "replacement child should have been spawned");
    // SAFETY: the retired heap keeps the object alive while refs drain.
    unsafe {
        let header = &*ActorHeap::header_of(ptr);
        assert!(
            header.foreign_count >= 1,
            "retired heap object must remain readable"
        );
    }
}

// ========================================================================
// SimpleOneForOne Dynamic Children Tests
// ========================================================================

/// Build a module declaring actor type `DynWorker` with a `count` state
/// field (default `default_count`) and one bytecode behavior
/// `DynWorker.inc` that increments `count` and returns the new value.
fn dyn_worker_module(default_count: i64) -> crate::bytecode::CodeModule {
    use crate::bytecode::{
        ActorMeta, BehaviorTableEntry, CodeModule, Constant, Instruction, OpCode,
    };

    let mut module = CodeModule::new("dyn_test");
    let field_idx = module.add_constant(Constant::String("count".to_string()));
    let one_idx = module.add_constant(Constant::Int(1));
    module.add_behavior(BehaviorTableEntry {
        name: "DynWorker.inc".to_string(),
        param_count: 0,
        code_offset: 0,
        local_count: 4,
        effect_mask: 0,
        compensate_offset: None,
        content_hash: None,
        source_location: None,
        parallel_branches: None,
    });
    module.emit(Instruction::new3(
        OpCode::StateGet,
        ((field_idx >> 8) & 0xFF) as u8,
        (field_idx & 0xFF) as u8,
        1,
    ));
    module.emit(Instruction::new3(
        OpCode::ConstU,
        ((one_idx >> 8) & 0xFF) as u8,
        (one_idx & 0xFF) as u8,
        2,
    ));
    module.emit(Instruction::new3(OpCode::IAdd, 1, 2, 3));
    module.emit(Instruction::new3(OpCode::StateSet, 0, 0, 3));
    module.emit(Instruction::new1(OpCode::RetVal, 3));
    module.add_actor_meta(ActorMeta {
        name: "DynWorker".to_string(),
        persistent: false,
        state_models: vec![("count".to_string(), crate::ast::StateModel::Local)],
        state_defaults: vec![("count".to_string(), Constant::Int(default_count))],
        behavior_indices: vec![0],
        type_hash: None,
        version: 1,
        migrations: String::new(),
        is_workflow: false,
        is_agent: false,
        is_organization: false,
        is_virtual: false,
        tools: vec![],
        semantic_memory_dimensions: None,
        procedural_memory_namespace: None,
        backend: crate::ast::ActorBackendKind::Native,
        fallback_config: String::new(),
        retry_config: String::new(),
    });
    module
}

#[test]
fn test_simple_one_for_one_start_child_spawns_real_children() {
    let mut rt = Runtime::new();
    let module = dyn_worker_module(0);
    let sup_id = rt.create_supervisor("pool", RestartStrategy::SimpleOneForOne);
    assert!(rt.set_supervisor_template(sup_id, "DynWorker", &module));

    let w1 = rt
        .start_supervised_child(sup_id, vec![])
        .expect("start_child should spawn from the template");
    let w2 = rt
        .start_supervised_child(sup_id, vec![])
        .expect("start_child should spawn from the template");
    assert_ne!(w1, w2);
    assert_eq!(rt.supervisors[&sup_id].child_count(), 2);
    assert_eq!(rt.actors[&w1].parent, Some(sup_id));
    assert_eq!(rt.actors[&w2].parent, Some(sup_id));

    // Children are real bytecode actors running the template behavior.
    assert_eq!(rt.ask_actor_sync(w1, 0, &[]).unwrap(), Value::int(1));
    assert_eq!(rt.ask_actor_sync(w2, 0, &[]).unwrap(), Value::int(1));

    // Distinct dynamic spec ids keep restart rate limiting per child.
    let specs: Vec<&str> = rt.supervisors[&sup_id]
        .children
        .iter()
        .map(|(s, _)| s.id.as_str())
        .collect();
    assert_eq!(specs, vec!["DynWorker_0", "DynWorker_1"]);
}

#[test]
fn test_simple_one_for_one_restart_from_template_on_crash() {
    let mut rt = Runtime::new();
    let module = dyn_worker_module(0);
    let sup_id = rt.create_supervisor("pool", RestartStrategy::SimpleOneForOne);
    assert!(rt.set_supervisor_template(sup_id, "DynWorker", &module));
    let w = rt.start_supervised_child(sup_id, vec![]).unwrap();

    // Mutate state away from the template defaults, then crash.
    assert_eq!(rt.ask_actor_sync(w, 0, &[]).unwrap(), Value::int(1));
    assert_eq!(rt.ask_actor_sync(w, 0, &[]).unwrap(), Value::int(2));
    rt.exit_actor(w, ExitReason::Error("crash".to_string()));

    assert!(!rt.actors.contains_key(&w));
    assert_eq!(rt.supervisors[&sup_id].child_count(), 1);
    let restarted = rt.supervisors[&sup_id].children[0].1;
    assert_ne!(restarted, w, "restart should create a fresh actor");
    assert_eq!(rt.actors[&restarted].parent, Some(sup_id));
    // The replacement restarts from the template defaults, not the
    // pre-crash state: its first inc returns 1, not 3.
    assert_eq!(rt.ask_actor_sync(restarted, 0, &[]).unwrap(), Value::int(1));
}

#[test]
fn test_simple_one_for_one_terminate_child_skips_restart() {
    let mut rt = Runtime::new();
    let module = dyn_worker_module(0);
    let sup_id = rt.create_supervisor("pool", RestartStrategy::SimpleOneForOne);
    assert!(rt.set_supervisor_template(sup_id, "DynWorker", &module));
    let w = rt.start_supervised_child(sup_id, vec![]).unwrap();
    assert_eq!(rt.supervisors[&sup_id].child_count(), 1);

    assert!(rt.terminate_supervised_child(sup_id, w));
    assert_eq!(
        rt.supervisors[&sup_id].child_count(),
        0,
        "terminated child must leave supervision"
    );
    assert!(
        !rt.actors.contains_key(&w),
        "terminated child must exit without a restart replacement"
    );
    // Unknown child / unknown supervisor are no-ops.
    assert!(!rt.terminate_supervised_child(sup_id, w));
    assert!(!rt.terminate_supervised_child(999_999, w));
}

#[test]
fn test_simple_one_for_one_normal_exit_not_restarted() {
    // Dynamic children are Transient: a Normal exit retires the child
    // without a replacement (unlike terminate_child, this routes through
    // the restart policy).
    let mut rt = Runtime::new();
    let module = dyn_worker_module(0);
    let sup_id = rt.create_supervisor("pool", RestartStrategy::SimpleOneForOne);
    assert!(rt.set_supervisor_template(sup_id, "DynWorker", &module));
    let w = rt.start_supervised_child(sup_id, vec![]).unwrap();

    rt.exit_actor(w, ExitReason::Normal);
    assert!(!rt.actors.contains_key(&w));
    assert_eq!(rt.supervisors[&sup_id].child_count(), 0);
}

#[test]
fn test_simple_one_for_one_start_child_guards() {
    let mut rt = Runtime::new();
    let module = dyn_worker_module(0);
    // No template set -> None.
    let sup_id = rt.create_supervisor("pool", RestartStrategy::SimpleOneForOne);
    assert_eq!(rt.start_supervised_child(sup_id, vec![]), None);
    // Non-dynamic strategy -> None even with a template set.
    let plain_id = rt.create_supervisor("plain", RestartStrategy::OneForOne);
    assert!(rt.set_supervisor_template(plain_id, "DynWorker", &module));
    assert_eq!(rt.start_supervised_child(plain_id, vec![]), None);
    // Unknown supervisor / unknown actor type -> None / false.
    assert_eq!(rt.start_supervised_child(999_999, vec![]), None);
    assert!(!rt.set_supervisor_template(sup_id, "NoSuchActor", &module));
    assert!(!rt.set_supervisor_template(999_999, "DynWorker", &module));
}

#[test]
fn test_otp_builtin_effect_strategy_mapping_and_noops() {
    use crate::bytecode::{CodeModule, Constant};

    let mut module = CodeModule::new("otp_test");
    let name_idx = module.add_constant(Constant::String("s".to_string())) as u32;

    let mut rt = Runtime::new();
    for (raw, want) in [
        (0i64, RestartStrategy::OneForOne),
        (1, RestartStrategy::OneForAll),
        (2, RestartStrategy::RestForOne),
        (3, RestartStrategy::SimpleOneForOne),
    ] {
        let id = rt
            .perform_otp_builtin(
                Some("create_supervisor"),
                &module,
                &[Value::string(name_idx), Value::int(raw)],
            )
            .and_then(|v| v.as_int())
            .expect("create_supervisor should return an Int id") as u64;
        assert_eq!(rt.supervisors[&id].strategy, want);
    }

    // Out-of-range strategy -> nil no-op (no supervisor created).
    let before = rt.supervisors.len();
    let value = rt.perform_otp_builtin(
        Some("create_supervisor"),
        &module,
        &[Value::string(name_idx), Value::int(9)],
    );
    assert_eq!(value, Some(Value::nil()));
    assert_eq!(rt.supervisors.len(), before);

    // Policy mapping via supervise_child (2 = transient).
    let sup_id = rt
        .perform_otp_builtin(
            Some("create_supervisor"),
            &module,
            &[Value::string(name_idx), Value::int(0)],
        )
        .and_then(|v| v.as_int())
        .unwrap() as u64;
    let child = rt.spawn_actor(Box::new(|| vec![]));
    let value = rt.perform_otp_builtin(
        Some("supervise_child"),
        &module,
        &[
            Value::int(sup_id as i64),
            Value::actor_ref(child),
            Value::int(2),
        ],
    );
    assert_eq!(value, Some(Value::nil()));
    assert_eq!(
        rt.supervisors[&sup_id].children[0].0.restart_policy,
        RestartPolicy::Transient
    );
    // supervise_child with an unknown supervisor is a nil no-op.
    let value = rt.perform_otp_builtin(
        Some("supervise_child"),
        &module,
        &[Value::int(999_999), Value::actor_ref(child), Value::int(0)],
    );
    assert_eq!(value, Some(Value::nil()));

    // Unknown op -> None (unhandled); child_count on unknown id -> nil.
    assert_eq!(rt.perform_otp_builtin(Some("bogus"), &module, &[]), None);
    assert_eq!(
        rt.perform_otp_builtin(Some("child_count"), &module, &[Value::int(999_999)]),
        Some(Value::nil())
    );
}

// ========================================================================
// ORCA GC Tests
// ========================================================================

#[test]
fn test_orca_ref_counting_basic() {
    let mut heap = ActorHeap::new(1024 * 1024);
    let mut gc = OrcaGc::new(1);
    let obj = gc.alloc_object(&mut heap, 64, TypeTag::Raw);
    assert!(obj.is_some());
    // local_count starts at 1 (creator holds one ref)
    let header_ptr = unsafe { heap.header_ptr(obj.unwrap()) };
    let local_count = unsafe { (*header_ptr).ref_count };
    assert_eq!(local_count, 1);

    unsafe { gc.local_ref(&heap, obj.unwrap()) };
    let local_count2 = unsafe { (*header_ptr).ref_count };
    assert_eq!(local_count2, 2);

    unsafe { gc.drop_local_ref(&mut heap, obj.unwrap()) };
    let local_count3 = unsafe { (*header_ptr).ref_count };
    assert_eq!(local_count3, 1);
}

#[test]
fn test_orca_cycle_detection() {
    // Cycle detection is handled by CycleDetector, not directly by OrcaGc.
    // This test verifies that two objects can be allocated and reference
    // each other via payload pointers (simulating a cycle).
    let mut heap = ActorHeap::new(1024 * 1024);
    let mut gc_a = OrcaGc::new(1);
    let a = gc_a.alloc_object(&mut heap, 64, TypeTag::Raw);
    let b = gc_a.alloc_object(&mut heap, 64, TypeTag::Raw);
    assert!(a.is_some());
    assert!(b.is_some());

    // Simulate cross-reference by storing pointers in payloads
    unsafe {
        let a_payload = a.unwrap();
        let b_payload = b.unwrap();
        std::ptr::write(a_payload as *mut *mut u8, b_payload);
        std::ptr::write(b_payload as *mut *mut u8, a_payload);
    }

    // Verify both objects are alive with ref count 1 each
    let header_a = unsafe { &*heap.header_ptr(a.unwrap()) };
    let header_b = unsafe { &*heap.header_ptr(b.unwrap()) };
    assert_eq!(header_a.ref_count, 1);
    assert_eq!(header_b.ref_count, 1);
}

// ========================================================================
// Distributed Tests
// ========================================================================

#[test]
fn test_distributed_send_local_fallback() {
    let mut rt = Runtime::new();
    let actor_id = rt.spawn_actor(Box::new(|| vec![("val".to_string(), Value::int(0))]));
    let local_addr = ActorAddress::Local { actor_id };
    rt.send_distributed(local_addr, "test", &[Value::int(42)]);
    assert!(rt.actors.contains_key(&actor_id));
}

#[test]
fn test_distributed_remote_address_local_fallback() {
    // A REMOTE address whose node is the local node (or a runtime with
    // distributed disabled) must deliver locally instead of silently
    // dropping — the single-node case of the SPEC2 known-issue list
    // (send/ask remote). `Runtime::send_distributed` resolves through
    // the distribution wrapper: distributed disabled → local delivery.
    let mut rt = Runtime::new();
    let actor_id = rt.spawn_actor(Box::new(|| vec![("val".to_string(), Value::int(0))]));

    // Distributed is disabled by default: a remote address still delivers.
    let remote_addr = ActorAddress::remote(NodeId::LOCAL, actor_id);
    rt.send_distributed(remote_addr, "test", &[Value::int(42)]);
    assert!(
        !rt.actors[&actor_id].mailbox.is_empty(),
        "remote-address send must fall back to local delivery"
    );

    // Same when the address names this node explicitly.
    let local_node = rt.distributed.node_id.unwrap_or(NodeId::LOCAL);
    let remote_addr = ActorAddress::remote(local_node, actor_id);
    rt.send_distributed(remote_addr, "test", &[Value::int(7)]);
    assert_eq!(
        rt.actors[&actor_id].mailbox.len(),
        2,
        "both remote-address sends must deliver locally"
    );
}

#[test]
fn test_crdt_merge_grow_only_counter() {
    let mut a = GCounter::new(1);
    a.increment_by(5);
    let mut b = GCounter::new(2);
    b.increment_by(3);
    a.merge(&b);
    // GCounter merge sums per-node increments: 5 + 3 = 8
    assert_eq!(a.value(), 8);
    let mut c = GCounter::new(3);
    c.increment_by(10);
    a.merge(&c);
    assert_eq!(a.value(), 18);
}

// ========================================================================
// v0.7 BEAM Primitive Tests
// ========================================================================

// -- Actor Name Registry (6 tests) --

#[test]
fn test_registry_register_and_whereis() {
    let mut rt = Runtime::new();
    let actor_id = rt.spawn_actor(Box::new(|| vec![]));
    assert!(rt.registry.register("my_actor", actor_id).is_ok());
    assert_eq!(rt.registry.whereis("my_actor"), Some(actor_id));
}

#[test]
fn test_registry_duplicate_name_fails() {
    let mut rt = Runtime::new();
    let a1 = rt.spawn_actor(Box::new(|| vec![]));
    let a2 = rt.spawn_actor(Box::new(|| vec![]));
    assert!(rt.registry.register("dup", a1).is_ok());
    assert!(rt.registry.register("dup", a2).is_err());
}

#[test]
fn test_registry_unregister() {
    let mut rt = Runtime::new();
    let actor_id = rt.spawn_actor(Box::new(|| vec![]));
    rt.registry.register("temp", actor_id).unwrap();
    assert!(rt.registry.unregister("temp").is_ok());
    assert_eq!(rt.registry.whereis("temp"), None);
}

#[test]
fn test_registry_registered_list() {
    let mut rt = Runtime::new();
    let a1 = rt.spawn_actor(Box::new(|| vec![]));
    let a2 = rt.spawn_actor(Box::new(|| vec![]));
    rt.registry.register("alpha", a1).unwrap();
    rt.registry.register("beta", a2).unwrap();
    let names = rt.registry.registered();
    assert!(names.contains(&"alpha".to_string()));
    assert!(names.contains(&"beta".to_string()));
}

#[test]
fn test_registry_cleanup_on_actor_exit() {
    let mut rt = Runtime::new();
    let actor_id = rt.spawn_actor(Box::new(|| vec![]));
    rt.registry.register("doomed", actor_id).unwrap();
    rt.exit_actor(actor_id, ExitReason::Normal);
    assert_eq!(rt.registry.whereis("doomed"), None);
}

#[test]
fn test_registry_invalid_name() {
    let mut rt = Runtime::new();
    let actor_id = rt.spawn_actor(Box::new(|| vec![]));
    assert!(rt.registry.register("", actor_id).is_err());
}

// -- Timer Wheel (5 tests) --

#[test]
fn test_timer_send_after() {
    let tw = TimerWheel::new();
    let timer_id = tw.send_after(Duration::from_millis(100), 42, 1, vec![]);
    assert_eq!(timer_id, TimerId(1));
    assert_eq!(tw.len(), 1);
}

#[test]
fn test_timer_cancel() {
    let tw = TimerWheel::new();
    let timer_id = tw.send_after(Duration::from_millis(100), 42, 1, vec![]);
    assert!(tw.cancel(timer_id));
    assert_eq!(tw.len(), 0);
}

#[test]
fn test_timer_tick_fires() {
    let tw = TimerWheel::new();
    let _ = tw.send_after(Duration::from_millis(0), 42, 99, vec![]);
    let fired = tw.tick(Instant::now() + Duration::from_millis(1000));
    assert_eq!(fired.len(), 1);
    assert_eq!(tw.len(), 0);
}

#[test]
fn test_timer_exit_after() {
    let tw = TimerWheel::new();
    let timer_id = tw.exit_after(Duration::from_millis(50), 42, "shutdown".to_string());
    assert_eq!(timer_id, TimerId(1));
    assert_eq!(tw.len(), 1);
}

#[test]
fn test_timer_kill_after() {
    let tw = TimerWheel::new();
    let timer_id = tw.kill_after(Duration::from_millis(50), 42);
    assert_eq!(timer_id, TimerId(1));
}

/// Regression: a timer still pending when the run queue drains must still
/// fire. run_scheduler used to break as soon as the queue emptied, so a
/// timer armed by an actor's last turn was silently dropped.
#[test]
fn test_run_scheduler_waits_for_pending_timer() {
    let mut rt = Runtime::new();
    let actor_id = rt.spawn_actor(Box::new(|| vec![("pings".to_string(), Value::int(0))]));
    {
        let actor = rt.actors.get_mut(&actor_id).unwrap();
        actor.register_behavior("ping", |actor, _args| {
            let n = actor
                .get_state_field("pings")
                .and_then(|v| v.as_int())
                .unwrap_or(0);
            actor.set_state_field("pings", Value::int(n + 1));
        });
    }
    // One direct message (processed immediately) plus a timer that
    // matures only after the queue has drained.
    rt.send_message(actor_id, "ping", &[]);
    let behavior_id = rt.behavior_id_for(actor_id, "ping").unwrap();
    rt.timer_wheel
        .send_after(Duration::from_millis(20), actor_id, behavior_id, vec![]);
    rt.run_scheduler();
    let pings = rt
        .actors
        .get(&actor_id)
        .unwrap()
        .get_state_field("pings")
        .and_then(|v| v.as_int());
    assert_eq!(
        pings,
        Some(2),
        "pending timer must fire before run_scheduler exits"
    );
}

// -- Timed selective receive (receive-after) wait-state tests --

/// The receive-wait timeout is armed exactly once per wait: a re-suspension
/// of the same wait must not restart the clock.
#[test]
fn test_receive_wait_timer_armed_once() {
    let mut rt = Runtime::new();
    let actor_id = rt.spawn_actor(Box::new(|| vec![]));

    rt.maybe_schedule_receive_wait(actor_id, Some(50));
    let first = rt.actors.get(&actor_id).unwrap().receive_wait;
    assert!(first.is_some(), "first suspend must arm the timeout");
    assert_eq!(rt.timer_wheel.len(), 1);

    // Re-suspending the same wait (e.g. a non-matching wake) keeps the
    // original timer instead of scheduling a fresh one.
    rt.maybe_schedule_receive_wait(actor_id, Some(5000));
    let second = rt.actors.get(&actor_id).unwrap().receive_wait;
    assert_eq!(first, second, "re-suspend must keep the original deadline");
    assert_eq!(rt.timer_wheel.len(), 1, "no second timer may be armed");
}

/// Non-positive (or absent) timeouts never arm a receive-wait timer: the
/// VM resolves those waits non-blockingly without suspending.
#[test]
fn test_receive_wait_timer_skips_nonpositive() {
    let mut rt = Runtime::new();
    let actor_id = rt.spawn_actor(Box::new(|| vec![]));

    rt.maybe_schedule_receive_wait(actor_id, Some(0));
    rt.maybe_schedule_receive_wait(actor_id, Some(-10));
    rt.maybe_schedule_receive_wait(actor_id, None);

    assert!(rt.actors.get(&actor_id).unwrap().receive_wait.is_none());
    assert!(rt.timer_wheel.is_empty());
}

/// Clearing a resolved wait cancels its pending timeout timer.
#[test]
fn test_clear_receive_wait_cancels_timer() {
    let mut rt = Runtime::new();
    let actor_id = rt.spawn_actor(Box::new(|| vec![]));

    rt.maybe_schedule_receive_wait(actor_id, Some(50));
    assert_eq!(rt.timer_wheel.len(), 1);

    rt.clear_receive_wait(actor_id);
    assert!(rt.actors.get(&actor_id).unwrap().receive_wait.is_none());
    assert!(
        rt.timer_wheel.is_empty(),
        "a resolved wait must not leave a timer behind"
    );
}

/// A timeout firing with no live suspension (e.g. the actor exited or the
/// wait already resolved) must drop the stale state instead of leaving a
/// poisoned timed-out marker for a later wait.
#[test]
fn test_fire_receive_wait_timeout_without_suspension_clears_state() {
    let mut rt = Runtime::new();
    let actor_id = rt.spawn_actor(Box::new(|| vec![]));

    rt.maybe_schedule_receive_wait(actor_id, Some(50));
    assert!(rt.actors.get(&actor_id).unwrap().receive_wait.is_some());

    rt.fire_receive_wait_timeout(actor_id);
    assert!(
        rt.actors.get(&actor_id).unwrap().receive_wait.is_none(),
        "stale receive-wait state must be cleared, not marked timed out"
    );
}

// -- Process Groups (5 tests) --

#[test]
fn test_pg_join_and_members() {
    let mut rt = Runtime::new();
    let actor_id = rt.spawn_actor(Box::new(|| vec![]));
    assert!(rt.process_groups.join("workers", actor_id).is_ok());
    let members = rt.process_groups.members("workers");
    assert!(members.contains(&actor_id));
}

#[test]
fn test_pg_leave() {
    let mut rt = Runtime::new();
    let actor_id = rt.spawn_actor(Box::new(|| vec![]));
    assert!(rt.process_groups.join("group", actor_id).is_ok());
    rt.process_groups.leave("group", actor_id);
    assert!(!rt.process_groups.is_member("group", actor_id));
}

#[test]
fn test_pg_leave_all() {
    let mut rt = Runtime::new();
    let a1 = rt.spawn_actor(Box::new(|| vec![]));
    assert!(rt.process_groups.join("g1", a1).is_ok());
    assert!(rt.process_groups.join("g2", a1).is_ok());
    rt.process_groups.leave_all(a1);
    assert!(rt.process_groups.members("g1").is_empty());
    assert!(rt.process_groups.members("g2").is_empty());
}

#[test]
fn test_pg_multiple_members() {
    let mut rt = Runtime::new();
    let a1 = rt.spawn_actor(Box::new(|| vec![]));
    let a2 = rt.spawn_actor(Box::new(|| vec![]));
    assert!(rt.process_groups.join("pool", a1).is_ok());
    assert!(rt.process_groups.join("pool", a2).is_ok());
    assert_eq!(rt.process_groups.member_count("pool"), 2);
}

#[test]
fn test_pg_join_idempotent() {
    let mut rt = Runtime::new();
    let actor_id = rt.spawn_actor(Box::new(|| vec![]));
    assert!(rt.process_groups.join("idempotent", actor_id).is_ok());
    assert!(rt.process_groups.join("idempotent", actor_id).is_ok());
    assert_eq!(rt.process_groups.member_count("idempotent"), 1);
}

// -- Links & Monitors (8 tests) --

#[test]
fn test_link_actors() {
    let mut rt = Runtime::new();
    let a = rt.spawn_actor(Box::new(|| vec![]));
    let b = rt.spawn_actor(Box::new(|| vec![]));
    rt.link_actors(a, b);
    assert!(rt.actors[&a].links.contains(&b));
    assert!(rt.actors[&b].links.contains(&a));
}

#[test]
fn test_unlink_actors() {
    let mut rt = Runtime::new();
    let a = rt.spawn_actor(Box::new(|| vec![]));
    let b = rt.spawn_actor(Box::new(|| vec![]));
    rt.link_actors(a, b);
    rt.unlink_actors(a, b);
    assert!(!rt.actors[&a].links.contains(&b));
    assert!(!rt.actors[&b].links.contains(&a));
}

#[test]
fn test_monitor_target() {
    let mut rt = Runtime::new();
    let watcher = rt.spawn_actor(Box::new(|| vec![]));
    let target = rt.spawn_actor(Box::new(|| vec![]));
    rt.current_actor = Some(watcher);
    rt.monitor(watcher, target);
    assert!(rt.actors[&target].monitors.contains(&watcher));
}

#[test]
fn test_demonitor() {
    let mut rt = Runtime::new();
    let watcher = rt.spawn_actor(Box::new(|| vec![]));
    let target = rt.spawn_actor(Box::new(|| vec![]));
    rt.monitor(watcher, target);
    rt.demonitor(watcher, target);
    assert!(!rt.actors[&target].monitors.contains(&watcher));
}

#[test]
fn test_monitor_sends_down_on_exit() {
    let mut rt = Runtime::new();
    let watcher = rt.spawn_actor(Box::new(|| vec![]));
    let target = rt.spawn_actor(Box::new(|| vec![]));
    rt.monitor(watcher, target);
    rt.exit_actor(target, ExitReason::Error("boom".to_string()));
    assert!(!rt.actors.contains_key(&target));
}

#[test]
fn test_exit_propagates_to_linked_actors() {
    let mut rt = Runtime::new();
    let a = rt.spawn_actor(Box::new(|| vec![]));
    let b = rt.spawn_actor(Box::new(|| vec![]));
    rt.link_actors(a, b);
    rt.exit_actor(a, ExitReason::Error("kaboom".to_string()));
    assert!(!rt.actors.contains_key(&a));
    assert!(
        !rt.actors.contains_key(&b),
        "linked actor b should also exit"
    );
}

#[test]
fn test_exit_does_not_propagate_for_normal_exit() {
    let mut rt = Runtime::new();
    let a = rt.spawn_actor(Box::new(|| vec![]));
    let b = rt.spawn_actor(Box::new(|| vec![]));
    rt.link_actors(a, b);
    rt.exit_actor(a, ExitReason::Normal);
    assert!(!rt.actors.contains_key(&a));
    assert!(
        rt.actors.contains_key(&b),
        "linked actor b should NOT exit on normal exit"
    );
}

#[test]
fn test_trap_exit_converts_to_message() {
    let mut rt = Runtime::new();
    let a = rt.spawn_actor(Box::new(|| vec![]));
    let b = rt.spawn_actor(Box::new(|| vec![]));
    rt.actors.get_mut(&b).unwrap().trap_exits = true;
    rt.link_actors(a, b);
    rt.exit_actor(a, ExitReason::Error("boom".to_string()));
    assert!(!rt.actors.contains_key(&a));
    assert!(
        rt.actors.contains_key(&b),
        "actor with trap_exits should survive"
    );
    assert!(
        !rt.actors[&b].mailbox.is_empty(),
        "exit signal should become message"
    );
}

// ========================================================================
// VM Opcode Tests
// ========================================================================

#[test]
fn test_vm_value_nan_tagging() {
    let v = Value::int(42);
    assert_eq!(v.as_int(), Some(42));
    let f = Value::float(2.5);
    assert!((f.as_float().unwrap() - 2.5).abs() < 0.001);
    assert_eq!(Value::bool(true).as_bool(), Some(true));
    assert!(Value::unit().is_unit());
}

#[test]
fn test_vm_frame_operations() {
    let frame = Frame::new(None, 0);
    assert!(frame.regs[0].is_nil());
    assert_eq!(frame.pc, 0);
}

#[test]
fn test_fresh_actor_id_increments() {
    let id1 = fresh_actor_id();
    let id2 = fresh_actor_id();
    assert_eq!(id2, id1 + 1);
}

#[test]
fn test_spawn_actor_near_colocates_on_shard() {
    // Single-shard: co-location is trivially satisfied (one shard).
    let mut rt = Runtime::new();
    let a = rt.spawn_actor(Box::new(|| vec![]));
    let b = rt.spawn_actor_near(a, Box::new(|| vec![]));
    assert!(rt.actors.contains_key(&b), "near-spawned actor exists");

    // Sharded: the child's id maps to the reference actor's shard, so
    // `actor_id % shard_count` ownership is preserved exactly.
    let shards = Runtime::new_sharded(4);
    let shard_count = shards.len() as u64;
    for mut shard in shards {
        let a = shard.spawn_actor(Box::new(|| vec![]));
        let b = shard.spawn_actor_near(a, Box::new(|| vec![]));
        assert_eq!(
            b % shard_count,
            a % shard_count,
            "spawn_actor_near must co-locate on the reference actor's shard"
        );
    }
}

// ========================================================================
// v0.7 Persistence Tests
// ========================================================================

#[test]
fn test_persistent_actor_snapshots_durable_state() {
    let mut rt = Runtime::new();
    let mut models = HashMap::new();
    models.insert("count".to_string(), StateModel::Durable);
    let actor_id = rt.spawn_persistent_actor(
        Box::new(|| vec![("count".to_string(), Value::int(0))]),
        models,
    );
    rt.actors
        .get_mut(&actor_id)
        .unwrap()
        .register_behavior("inc", |actor, _args| {
            if let Some(n) = actor.get_state_field("count").and_then(|v| v.as_int()) {
                actor.set_state_field("count", Value::int(n + 1));
            }
        });

    rt.send_message(actor_id, "inc", &[]);
    rt.step_actor(actor_id);

    let snapshot = rt.persistence.load_snapshot(actor_id).unwrap();
    assert_eq!(snapshot.state.get("count"), Some(&PersistedValue::Int(1)));
    assert!(snapshot.sequence > 0);
}

#[test]
fn test_persistent_actor_recovers_from_snapshot() {
    let mut rt = Runtime::new();
    let mut models = HashMap::new();
    models.insert("count".to_string(), StateModel::Durable);
    let actor_id = rt.spawn_persistent_actor(
        Box::new(|| vec![("count".to_string(), Value::int(0))]),
        models,
    );
    rt.actors
        .get_mut(&actor_id)
        .unwrap()
        .register_behavior("inc", |actor, _args| {
            if let Some(n) = actor.get_state_field("count").and_then(|v| v.as_int()) {
                actor.set_state_field("count", Value::int(n + 1));
            }
        });

    // Process 3 increments.
    for _ in 0..3 {
        rt.send_message(actor_id, "inc", &[]);
        rt.step_actor(actor_id);
    }

    // Simulate node death: drop the actor from memory but keep the store.
    rt.actors.remove(&actor_id);

    // Recover and verify state is replayed.
    rt.recover_actor(actor_id).unwrap();
    let count = rt
        .actors
        .get(&actor_id)
        .unwrap()
        .get_state_field("count")
        .and_then(|v| v.as_int())
        .unwrap();
    assert_eq!(count, 3);
}

#[test]
fn test_persistent_string_state_survives_checkpoint_and_recovery() {
    use crate::bytecode::{CodeModule, Constant};

    let mut rt = Runtime::new();
    let mut models = HashMap::new();
    models.insert("greeting".to_string(), StateModel::Durable);
    let actor_id = rt.spawn_persistent_actor(
        Box::new(|| vec![("greeting".to_string(), Value::int(0))]),
        models,
    );

    // Set up a module with a string constant so from_value_resolved can resolve it.
    let mut module = CodeModule::new("test");
    let hello_idx = module.add_constant(Constant::String("hello world".to_string()));
    let string_val = Value::string(hello_idx as u32);

    // Set the string value and module on the actor.
    {
        let actor = rt.actors.get_mut(&actor_id).unwrap();
        actor.set_state_field("greeting", string_val);
        actor.bytecode_module = Some(module);
    }

    // Force a checkpoint.
    rt.checkpoint_actor(actor_id);

    // Verify the snapshot contains the string, not nil.
    let snapshot = rt.persistence.load_snapshot(actor_id).unwrap();
    assert_eq!(
        snapshot.state.get("greeting"),
        Some(&PersistedValue::String("hello world".to_string())),
        "string state must be preserved as PersistedValue::String, not Nil"
    );

    // Simulate node death and recovery.
    rt.actors.remove(&actor_id);
    rt.recover_actor(actor_id).unwrap();

    // After recovery, the string value should be restored on the actor heap.
    let actor = rt.actors.get_mut(&actor_id).unwrap();
    let restored = actor.get_state_field("greeting").unwrap();
    assert!(!restored.is_nil(), "restored string must not be nil");
}
#[test]
fn test_local_state_is_not_persisted() {
    let mut rt = Runtime::new();
    let mut models = HashMap::new();
    models.insert("temp".to_string(), StateModel::Local);
    let actor_id = rt.spawn_persistent_actor(
        Box::new(|| vec![("temp".to_string(), Value::int(42))]),
        models,
    );
    rt.actors
        .get_mut(&actor_id)
        .unwrap()
        .register_behavior("set", |actor, args| {
            if let Some(n) = args.get(0).and_then(|v| v.as_int()) {
                actor.set_state_field("temp", Value::int(n));
            }
        });

    rt.send_message(actor_id, "set", &[Value::int(99)]);
    rt.step_actor(actor_id);

    let snapshot = rt.persistence.load_snapshot(actor_id).unwrap();
    assert!(!snapshot.state.contains_key("temp"));
}

#[test]
fn test_event_sourced_counter_replays_from_event_log() {
    let mut rt = Runtime::new();
    rt.persistence = Box::new(MemoryStore::new());

    let mut models = HashMap::new();
    models.insert("counter".to_string(), StateModel::EventSourced);
    let actor_id = rt.spawn_persistent_actor(
        Box::new(|| vec![("counter".to_string(), Value::int(0))]),
        models,
    );

    for i in 0..5 {
        rt.emit_event(actor_id, "Incremented", &[Value::int(i)]);
    }

    let count = rt.actors.get(&actor_id).unwrap().get_state_field("counter");
    assert_eq!(
        count,
        Some(Value::int(5)),
        "counter should be 5 after 5 events"
    );

    rt.checkpoint_actor(actor_id);
    let snapshot = rt.persistence.load_snapshot(actor_id).unwrap();
    assert_eq!(
        snapshot.state.contains_key("counter"),
        false,
        "EventSourced field must not appear in snapshot"
    );

    let events = rt.persistence.read_events(actor_id);
    assert_eq!(events.len(), 5, "5 events must be persisted");
    assert_eq!(events[0].field_name, "counter");
    assert_eq!(events[0].event_name, "Incremented");

    rt.actors.remove(&actor_id);
    let recovered_id = rt.recover_actor(actor_id).unwrap();
    assert_eq!(recovered_id, actor_id);

    let recovered_count = rt.actors.get(&actor_id).unwrap().get_state_field("counter");
    assert_eq!(
        recovered_count,
        Some(Value::int(5)),
        "recovered actor must have counter=5 from event replay"
    );
}

#[test]
fn test_memory_store_latest_sequence() {
    let mut store = MemoryStore::new();
    let snapshot = ActorSnapshot {
        actor_id: 1,
        sequence: 5,
        state: HashMap::new(),
        waiting_signal: None,
        crdt_snapshot: None,
    };
    store.save_snapshot(snapshot).unwrap();
    store
        .append_journal(
            1,
            JournalEntry {
                sequence: 7,
                behavior_id: 0,
                payload: vec![],
            },
        )
        .unwrap();
    assert_eq!(store.latest_sequence(1), 7);
}

#[cfg(feature = "sqlite")]
#[test]
fn test_libsql_store_save_load_snapshot() {
    let mut store = LibsqlStore::in_memory().unwrap();
    let mut state = HashMap::new();
    state.insert("count".to_string(), PersistedValue::Int(42));
    let snapshot = ActorSnapshot {
        actor_id: 1,
        sequence: 3,
        state,
        waiting_signal: None,
        crdt_snapshot: None,
    };
    store.save_snapshot(snapshot).unwrap();

    let loaded = store.load_snapshot(1).unwrap();
    assert_eq!(loaded.actor_id, 1);
    assert_eq!(loaded.sequence, 3);
    assert_eq!(loaded.state.get("count"), Some(&PersistedValue::Int(42)));
}

#[cfg(feature = "sqlite")]
#[test]
fn test_libsql_store_append_read_journal() {
    let mut store = LibsqlStore::in_memory().unwrap();
    store
        .append_journal(
            1,
            JournalEntry {
                sequence: 1,
                behavior_id: 0,
                payload: vec![PersistedValue::Int(10)],
            },
        )
        .unwrap();
    store
        .append_journal(
            1,
            JournalEntry {
                sequence: 2,
                behavior_id: 1,
                payload: vec![PersistedValue::Int(20)],
            },
        )
        .unwrap();

    let journal = store.read_journal(1);
    assert_eq!(journal.len(), 2);
    assert_eq!(journal[0].sequence, 1);
    assert_eq!(journal[1].behavior_id, 1);
    assert_eq!(journal[1].payload, vec![PersistedValue::Int(20)]);
}

#[cfg(feature = "sqlite")]
#[test]
fn test_libsql_store_latest_sequence() {
    let mut store = LibsqlStore::in_memory().unwrap();
    store
        .save_snapshot(ActorSnapshot {
            actor_id: 1,
            sequence: 5,
            state: HashMap::new(),
            waiting_signal: None,
            crdt_snapshot: None,
        })
        .unwrap();
    store
        .append_journal(
            1,
            JournalEntry {
                sequence: 7,
                behavior_id: 0,
                payload: vec![],
            },
        )
        .unwrap();
    assert_eq!(store.latest_sequence(1), 7);
}

#[cfg(feature = "sqlite")]
#[test]
fn test_libsql_store_clear() {
    let mut store = LibsqlStore::in_memory().unwrap();
    store
        .save_snapshot(ActorSnapshot {
            actor_id: 1,
            sequence: 1,
            state: HashMap::new(),
            waiting_signal: None,
            crdt_snapshot: None,
        })
        .unwrap();
    store
        .append_journal(
            1,
            JournalEntry {
                sequence: 2,
                behavior_id: 0,
                payload: vec![],
            },
        )
        .unwrap();

    store.clear(1).unwrap();
    assert!(store.load_snapshot(1).is_none());
    assert!(store.read_journal(1).is_empty());
    assert_eq!(store.latest_sequence(1), 0);
}

#[cfg(feature = "sqlite")]
#[test]
fn test_libsql_store_persists_to_disk() {
    let path = std::env::temp_dir().join(format!("nulang_libsql_test_{}.db", std::process::id()));
    {
        let mut store = LibsqlStore::new(&path).unwrap();
        let mut state = HashMap::new();
        state.insert("x".to_string(), PersistedValue::Float(1.5));
        store
            .save_snapshot(ActorSnapshot {
                actor_id: 1,
                sequence: 1,
                state,
                waiting_signal: None,
                crdt_snapshot: None,
            })
            .unwrap();
        store
            .append_journal(
                1,
                JournalEntry {
                    sequence: 2,
                    behavior_id: 0,
                    payload: vec![PersistedValue::Bool(true)],
                },
            )
            .unwrap();
    }

    {
        let store = LibsqlStore::new(&path).unwrap();
        let snapshot = store.load_snapshot(1).unwrap();
        assert_eq!(snapshot.sequence, 1);
        assert_eq!(snapshot.state.get("x"), Some(&PersistedValue::Float(1.5)));
        let journal = store.read_journal(1);
        assert_eq!(journal.len(), 1);
        assert_eq!(journal[0].payload, vec![PersistedValue::Bool(true)]);
    }

    let _ = std::fs::remove_file(&path);
}

#[cfg(feature = "sqlite")]
#[test]
fn test_libsql_store_crdt_snapshot_roundtrip() {
    let mut store = LibsqlStore::in_memory().unwrap();
    store
        .save_snapshot(ActorSnapshot {
            actor_id: 1,
            sequence: 3,
            state: HashMap::new(),
            waiting_signal: None,
            crdt_snapshot: Some(vec![(7, 1, vec![1, 2, 3]), (8, 2, vec![])]),
        })
        .unwrap();

    let loaded = store.load_snapshot(1).unwrap();
    assert_eq!(
        loaded.crdt_snapshot,
        Some(vec![(7, 1, vec![1, 2, 3]), (8, 2, vec![])])
    );

    // Saving a snapshot without CRDT state must clear the stored column.
    store
        .save_snapshot(ActorSnapshot {
            actor_id: 1,
            sequence: 4,
            state: HashMap::new(),
            waiting_signal: None,
            crdt_snapshot: None,
        })
        .unwrap();
    let loaded = store.load_snapshot(1).unwrap();
    assert!(loaded.crdt_snapshot.is_none());
}

#[cfg(feature = "sqlite")]
#[test]
fn test_libsql_store_migrates_old_schema_crdt_column() {
    let path =
        std::env::temp_dir().join(format!("nulang_libsql_migrate_{}.db", std::process::id()));
    {
        // Create a database with the pre-crdt_snapshot four-column schema.
        let rt = tokio::runtime::Runtime::new().unwrap();
        rt.block_on(async {
            let db = libsql::Builder::new_local(&path).build().await.unwrap();
            let conn = db.connect().unwrap();
            conn.execute(
                "CREATE TABLE snapshots (
                    actor_id INTEGER PRIMARY KEY,
                    sequence INTEGER NOT NULL,
                    state TEXT NOT NULL,
                    waiting_signal TEXT
                )",
                (),
            )
            .await
            .unwrap();
        });
    }

    {
        // LibsqlStore::new must ALTER the table to add crdt_snapshot.
        let mut store = LibsqlStore::new(&path).unwrap();
        store
            .save_snapshot(ActorSnapshot {
                actor_id: 1,
                sequence: 3,
                state: HashMap::new(),
                waiting_signal: None,
                crdt_snapshot: Some(vec![(7, 1, vec![1, 2, 3])]),
            })
            .unwrap();
        let loaded = store.load_snapshot(1).unwrap();
        assert_eq!(loaded.crdt_snapshot, Some(vec![(7, 1, vec![1, 2, 3])]));
    }

    let _ = std::fs::remove_file(&path);
}

#[cfg(feature = "sqlite")]
#[test]
fn test_libsql_store_wal_and_synchronous_pragmas() {
    let path = std::env::temp_dir().join(format!("nulang_libsql_pragma_{}.db", std::process::id()));
    {
        // Default constructor: WAL journal + synchronous=FULL.
        let store = LibsqlStore::new(&path).unwrap();
        let journal_mode = store.query("PRAGMA journal_mode", &[]).unwrap();
        assert_eq!(
            journal_mode.first().map(|s| s.as_str()),
            Some("[\"wal\"]"),
            "journal_mode rows: {:?}",
            journal_mode
        );
        let synchronous = store.query("PRAGMA synchronous", &[]).unwrap();
        assert_eq!(
            synchronous.first().map(|s| s.as_str()),
            Some("[2]"),
            "synchronous rows: {:?}",
            synchronous
        );
    }
    {
        // synchronous is per-connection (WAL persists in the file); an
        // explicit Normal mode must read back as 1.
        let store = LibsqlStore::with_sync_mode(&path, SqliteSyncMode::Normal).unwrap();
        let journal_mode = store.query("PRAGMA journal_mode", &[]).unwrap();
        assert_eq!(
            journal_mode.first().map(|s| s.as_str()),
            Some("[\"wal\"]"),
            "journal_mode rows after reopen: {:?}",
            journal_mode
        );
        let synchronous = store.query("PRAGMA synchronous", &[]).unwrap();
        assert_eq!(
            synchronous.first().map(|s| s.as_str()),
            Some("[1]"),
            "synchronous rows after reopen: {:?}",
            synchronous
        );
    }

    let _ = std::fs::remove_file(&path);
}

#[cfg(feature = "sqlite")]
#[test]
fn test_persistent_actor_with_libsql_store() {
    let mut rt = Runtime::new();
    rt.persistence = Box::new(LibsqlStore::in_memory().unwrap());
    let mut models = HashMap::new();
    models.insert("count".to_string(), StateModel::Durable);
    let actor_id = rt.spawn_persistent_actor(
        Box::new(|| vec![("count".to_string(), Value::int(0))]),
        models,
    );
    rt.actors
        .get_mut(&actor_id)
        .unwrap()
        .register_behavior("inc", |actor, _args| {
            if let Some(n) = actor.get_state_field("count").and_then(|v| v.as_int()) {
                actor.set_state_field("count", Value::int(n + 1));
            }
        });

    for _ in 0..3 {
        rt.send_message(actor_id, "inc", &[]);
        rt.step_actor(actor_id);
    }

    let snapshot = rt.persistence.load_snapshot(actor_id).unwrap();
    assert_eq!(snapshot.state.get("count"), Some(&PersistedValue::Int(3)));
}

// ========================================================================
// VM / Runtime wiring tests (v0.7)
// ========================================================================

#[test]
fn test_vm_spawn_creates_persistent_actor() {
    use crate::bytecode::{
        ActorMeta, BehaviorTableEntry, CodeModule, Constant, Instruction, OpCode,
    };
    use crate::runtime::persistence::StateModel as RuntimeStateModel;
    use crate::vm::{Value, VM};
    use std::cell::RefCell;
    use std::rc::Rc;

    let mut module = CodeModule::new("test");
    module.add_actor_meta(ActorMeta {
        name: "Account".to_string(),
        persistent: true,
        state_models: vec![("balance".to_string(), crate::ast::StateModel::Durable)],
        state_defaults: vec![("balance".to_string(), Constant::Int(100))],
        behavior_indices: vec![0],
        type_hash: None,
        version: 1,
        migrations: String::new(),
        is_workflow: false,
        is_agent: false,
        is_organization: false,
        is_virtual: false,
        tools: vec![],
        semantic_memory_dimensions: None,
        procedural_memory_namespace: None,
        backend: crate::ast::ActorBackendKind::Native,
        fallback_config: String::new(),
        retry_config: String::new(),
    });
    module.add_behavior(BehaviorTableEntry {
        name: "Account.get".to_string(),
        param_count: 0,
        code_offset: 0,
        local_count: 1,
        effect_mask: 0,
        compensate_offset: None,
        content_hash: None,
        source_location: None,
        parallel_branches: None,
    });
    module.emit(Instruction::new3(OpCode::Spawn, 0, 0, 0));
    module.emit(Instruction::new0(OpCode::Halt));
    module.entry_point = Some(0);

    let rt = Rc::new(RefCell::new(Runtime::new()));
    let mut vm = VM::new();
    vm.set_actor_callbacks(Box::new(RuntimeVmCallbacks::new(rt.clone())));
    vm.load_module(module);
    let result = vm.run().unwrap();

    let actor_id = result.as_actor_id().expect("expected actor reference");
    assert_ne!(actor_id, 0);

    let rt_ref = rt.borrow();
    let actor = rt_ref.actors.get(&actor_id).expect("actor should exist");
    assert!(actor.persistent);
    assert_eq!(actor.get_state_field("balance"), Some(Value::int(100)));
    assert_eq!(
        actor.state_models.get("balance"),
        Some(&RuntimeStateModel::Durable)
    );
}

#[test]
fn test_vm_spawn_creates_non_persistent_actor() {
    use crate::bytecode::{
        ActorMeta, BehaviorTableEntry, CodeModule, Constant, Instruction, OpCode,
    };
    use crate::vm::{Value, VM};
    use std::cell::RefCell;
    use std::rc::Rc;

    let mut module = CodeModule::new("test");
    module.add_actor_meta(ActorMeta {
        name: "Counter".to_string(),
        persistent: false,
        state_models: vec![("count".to_string(), crate::ast::StateModel::Local)],
        state_defaults: vec![("count".to_string(), Constant::Int(0))],
        behavior_indices: vec![0],
        type_hash: None,
        version: 1,
        migrations: String::new(),
        is_workflow: false,
        is_agent: false,
        is_organization: false,
        is_virtual: false,
        tools: vec![],
        semantic_memory_dimensions: None,
        procedural_memory_namespace: None,
        backend: crate::ast::ActorBackendKind::Native,
        fallback_config: String::new(),
        retry_config: String::new(),
    });
    module.add_behavior(BehaviorTableEntry {
        name: "Counter.inc".to_string(),
        param_count: 0,
        code_offset: 0,
        local_count: 1,
        effect_mask: 0,
        compensate_offset: None,
        content_hash: None,
        source_location: None,
        parallel_branches: None,
    });
    module.emit(Instruction::new3(OpCode::Spawn, 0, 0, 0));
    module.emit(Instruction::new0(OpCode::Halt));
    module.entry_point = Some(0);

    let rt = Rc::new(RefCell::new(Runtime::new()));
    let mut vm = VM::new();
    vm.set_actor_callbacks(Box::new(RuntimeVmCallbacks::new(rt.clone())));
    vm.load_module(module);
    let result = vm.run().unwrap();

    let actor_id = result.as_actor_id().expect("expected actor reference");
    let rt_ref = rt.borrow();
    let actor = rt_ref.actors.get(&actor_id).unwrap();
    assert!(!actor.persistent);
    assert_eq!(actor.get_state_field("count"), Some(Value::int(0)));
}

#[test]
fn test_vm_arr_alloc_uses_actor_heap() {
    use crate::bytecode::{CodeModule, Constant, Instruction, OpCode};
    use crate::runtime::heap::{ActorHeap, TypeTag};
    use crate::vm::VM;
    use std::cell::RefCell;
    use std::rc::Rc;

    let rt = Rc::new(RefCell::new(Runtime::new()));
    let actor_id = rt.borrow_mut().spawn_actor(Box::new(|| vec![]));
    rt.borrow_mut().current_actor = Some(actor_id);

    let mut module = CodeModule::new("test");
    let len_idx = module.add_constant(Constant::Int(4));
    module.emit(Instruction::new3(
        OpCode::ConstU,
        ((len_idx >> 8) & 0xFF) as u8,
        (len_idx & 0xFF) as u8,
        1,
    ));
    module.emit(Instruction::new2(OpCode::ArrAlloc, 1, 0));
    module.emit(Instruction::new0(OpCode::Halt));
    module.entry_point = Some(0);

    let mut vm = VM::new();
    vm.set_actor_callbacks(Box::new(RuntimeVmCallbacks::new(rt.clone())));
    vm.load_module(module);
    vm.run().unwrap();

    let rt_ref = rt.borrow();
    let actor = rt_ref.actors.get(&actor_id).unwrap();
    assert_eq!(actor.heap.live_count(), 1);
    let mut ptrs = Vec::new();
    actor
        .heap
        .iter_live_objects(|_h, payload, _size| ptrs.push(payload));
    assert_eq!(ptrs.len(), 1);
    unsafe {
        let header = &*ActorHeap::header_of(ptrs[0]);
        assert_eq!(header.type_tag, TypeTag::Array);
    }
}

#[test]
fn test_vm_arr_load_store_and_len_on_actor_heap() {
    use crate::bytecode::{CodeModule, Constant, Instruction, OpCode};
    use crate::vm::VM;
    use std::cell::RefCell;
    use std::rc::Rc;

    let rt = Rc::new(RefCell::new(Runtime::new()));
    let actor_id = rt.borrow_mut().spawn_actor(Box::new(|| vec![]));
    rt.borrow_mut().current_actor = Some(actor_id);

    let mut module = CodeModule::new("test");
    let len_idx = module.add_constant(Constant::Int(3));
    let idx_idx = module.add_constant(Constant::Int(1));
    let val_idx = module.add_constant(Constant::Int(42));

    module.emit(Instruction::new3(
        OpCode::ConstU,
        ((len_idx >> 8) & 0xFF) as u8,
        (len_idx & 0xFF) as u8,
        1,
    ));
    module.emit(Instruction::new2(OpCode::ArrAlloc, 1, 0)); // r0 = arr
    module.emit(Instruction::new3(
        OpCode::ConstU,
        ((idx_idx >> 8) & 0xFF) as u8,
        (idx_idx & 0xFF) as u8,
        2,
    ));
    module.emit(Instruction::new3(
        OpCode::ConstU,
        ((val_idx >> 8) & 0xFF) as u8,
        (val_idx & 0xFF) as u8,
        3,
    ));
    module.emit(Instruction::new3(OpCode::ArrStore, 0, 2, 3));
    module.emit(Instruction::new3(OpCode::ArrLoad, 0, 2, 4));
    module.emit(Instruction::new3(OpCode::ArrLen, 0, 0, 5)); // r5 = len
    module.emit(Instruction::new2(OpCode::Move, 4, 0)); // return loaded value
    module.emit(Instruction::new0(OpCode::Halt));
    module.entry_point = Some(0);

    let mut vm = VM::new();
    vm.set_actor_callbacks(Box::new(RuntimeVmCallbacks::new(rt.clone())));
    vm.load_module(module);
    let result = vm.run().unwrap();

    assert_eq!(result.as_int(), Some(42));
}

#[test]
fn test_vm_drop_frees_actor_heap_object() {
    use crate::bytecode::{CodeModule, Constant, Instruction, OpCode};
    use crate::vm::VM;
    use std::cell::RefCell;
    use std::rc::Rc;

    let rt = Rc::new(RefCell::new(Runtime::new()));
    let actor_id = rt.borrow_mut().spawn_actor(Box::new(|| vec![]));
    rt.borrow_mut().current_actor = Some(actor_id);

    let mut module = CodeModule::new("test");
    let len_idx = module.add_constant(Constant::Int(4));
    module.emit(Instruction::new3(
        OpCode::ConstU,
        ((len_idx >> 8) & 0xFF) as u8,
        (len_idx & 0xFF) as u8,
        1,
    ));
    module.emit(Instruction::new2(OpCode::ArrAlloc, 1, 0));
    module.emit(Instruction::new1(OpCode::Drop, 0));
    module.emit(Instruction::new0(OpCode::Halt));
    module.entry_point = Some(0);

    let mut vm = VM::new();
    vm.set_actor_callbacks(Box::new(RuntimeVmCallbacks::new(rt.clone())));
    vm.load_module(module);
    vm.run().unwrap();

    let rt_ref = rt.borrow();
    let actor = rt_ref.actors.get(&actor_id).unwrap();
    assert_eq!(actor.heap.live_count(), 0);
}

/// Regression test: capturing a heap pointer into a closure (`CapStore`)
/// must retain it, so dropping the original local binding does not free an
/// object the closure still holds — a latent use-after-free that would
/// trigger the moment any codegen path emits `Drop` for a captured local.
#[test]
fn test_closure_capture_retains_heap_object_across_drop() {
    use crate::bytecode::{CodeModule, Constant, Instruction, OpCode};
    use crate::vm::VM;
    use std::cell::RefCell;
    use std::rc::Rc;

    let rt = Rc::new(RefCell::new(Runtime::new()));
    let actor_id = rt.borrow_mut().spawn_actor(Box::new(|| vec![]));
    rt.borrow_mut().current_actor = Some(actor_id);

    let mut module = CodeModule::new("test_capture_retain");
    let len_idx = module.add_constant(Constant::Int(4));

    // main:
    //   0: ConstU 4 -> r2 (array length)
    //   1: ArrAlloc r2 -> r1 (heap object, local ref_count starts at 1)
    //   2: Closure #0 -> r3
    //   3: CapStore r3[0] = r1   (must retain: ref_count -> 2)
    //   4: Drop r1               (ref_count -> 1; must NOT free)
    //   5: Move r3 -> r4
    //   6: Call r4, 0 args, dst r0
    //   7: Halt
    // fn0 (at offset 8): just returns unit; the object's survival is
    // checked on the actor heap directly after run() completes.
    module.emit(Instruction::new3(
        OpCode::ConstU,
        ((len_idx >> 8) & 0xFF) as u8,
        (len_idx & 0xFF) as u8,
        2,
    )); // 0
    module.emit(Instruction::new2(OpCode::ArrAlloc, 2, 1)); // 1
    module.emit(Instruction::new3(OpCode::Closure, 0, 0, 3)); // 2
    module.emit(Instruction::new3(OpCode::CapStore, 3, 0, 1)); // 3
    module.emit(Instruction::new1(OpCode::Drop, 1)); // 4
    module.emit(Instruction::new2(OpCode::Move, 3, 4)); // 5
    module.emit(Instruction::new3(OpCode::Call, 4, 0, 0)); // 6
    module.emit(Instruction::new0(OpCode::Halt)); // 7
    let fn0_offset = module.current_offset();
    module.emit(Instruction::new1(OpCode::Const0, 0)); // 8
    module.emit(Instruction::new1(OpCode::RetVal, 0)); // 9
    module.function_table.push(fn0_offset);
    module.entry_point = Some(0);

    let mut vm = VM::new();
    vm.set_actor_callbacks(Box::new(RuntimeVmCallbacks::new(rt.clone())));
    vm.load_module(module);
    vm.run().unwrap();

    let rt_ref = rt.borrow();
    let actor = rt_ref.actors.get(&actor_id).unwrap();
    assert_eq!(
        actor.heap.live_count(),
        1,
        "object captured by a closure must survive a Drop of the original local"
    );
}

/// Same regression as `test_closure_capture_retains_heap_object_across_drop`,
/// but for `ArrStore`: storing a heap pointer into an array slot must retain
/// it too, or a later `Drop` of the value's original binding would free it
/// out from under the array — a latent use-after-free CapStore was already
/// protected against but ArrStore wasn't.
#[test]
fn test_array_store_retains_heap_object_across_drop() {
    use crate::bytecode::{CodeModule, Constant, Instruction, OpCode};
    use crate::vm::VM;
    use std::cell::RefCell;
    use std::rc::Rc;

    let rt = Rc::new(RefCell::new(Runtime::new()));
    let actor_id = rt.borrow_mut().spawn_actor(Box::new(|| vec![]));
    rt.borrow_mut().current_actor = Some(actor_id);

    let mut module = CodeModule::new("test_arrstore_retain");
    let inner_len_idx = module.add_constant(Constant::Int(2));
    let outer_len_idx = module.add_constant(Constant::Int(3));

    // main:
    //   0: ConstU 2 -> r1 (inner array length)
    //   1: ArrAlloc r1 -> r2 (inner object, local ref_count starts at 1)
    //   2: ConstU 3 -> r3 (outer array length)
    //   3: ArrAlloc r3 -> r4 (outer array object)
    //   4: Const0 -> r5 (index 0)
    //   5: ArrStore r4[0] = r2 (must retain r2: ref_count -> 2)
    //   6: Drop r2 (ref_count -> 1; must NOT free)
    //   7: Halt
    module.emit(Instruction::new3(
        OpCode::ConstU,
        ((inner_len_idx >> 8) & 0xFF) as u8,
        (inner_len_idx & 0xFF) as u8,
        1,
    )); // 0
    module.emit(Instruction::new2(OpCode::ArrAlloc, 1, 2)); // 1
    module.emit(Instruction::new3(
        OpCode::ConstU,
        ((outer_len_idx >> 8) & 0xFF) as u8,
        (outer_len_idx & 0xFF) as u8,
        3,
    )); // 2
    module.emit(Instruction::new2(OpCode::ArrAlloc, 3, 4)); // 3
    module.emit(Instruction::new1(OpCode::Const0, 5)); // 4
    module.emit(Instruction::new3(OpCode::ArrStore, 4, 5, 2)); // 5
    module.emit(Instruction::new1(OpCode::Drop, 2)); // 6
    module.emit(Instruction::new0(OpCode::Halt)); // 7
    module.entry_point = Some(0);

    let mut vm = VM::new();
    vm.set_actor_callbacks(Box::new(RuntimeVmCallbacks::new(rt.clone())));
    vm.load_module(module);
    vm.run().unwrap();

    let rt_ref = rt.borrow();
    let actor = rt_ref.actors.get(&actor_id).unwrap();
    assert_eq!(
        actor.heap.live_count(),
        2,
        "object stored into an array slot must survive a Drop of the original local (both the inner and outer objects should remain live)"
    );
}

/// Same regression as `test_array_store_retains_heap_object_across_drop`,
/// but for `RecS`: storing a heap pointer into a record field must retain
/// it too, mirroring CapStore/ArrStore.
#[test]
fn test_record_field_store_retains_heap_object_across_drop() {
    use crate::bytecode::{CodeModule, Constant, Instruction, OpCode};
    use crate::vm::VM;
    use std::cell::RefCell;
    use std::rc::Rc;

    let rt = Rc::new(RefCell::new(Runtime::new()));
    let actor_id = rt.borrow_mut().spawn_actor(Box::new(|| vec![]));
    rt.borrow_mut().current_actor = Some(actor_id);

    let mut module = CodeModule::new("test_recs_retain");
    let inner_len_idx = module.add_constant(Constant::Int(2));

    // main:
    //   0: ConstU 2 -> r1 (inner array length)
    //   1: ArrAlloc r1 -> r2 (inner object, local ref_count starts at 1)
    //   2: RecMk 1 slot -> r3 (record object)
    //   3: RecS r3[field 0] = r2 (must retain r2: ref_count -> 2)
    //   4: Drop r2 (ref_count -> 1; must NOT free)
    //   5: Halt
    module.emit(Instruction::new3(
        OpCode::ConstU,
        ((inner_len_idx >> 8) & 0xFF) as u8,
        (inner_len_idx & 0xFF) as u8,
        1,
    )); // 0
    module.emit(Instruction::new2(OpCode::ArrAlloc, 1, 2)); // 1
    module.emit(Instruction::new2(OpCode::RecMk, 1, 3)); // 2
    module.emit(Instruction::new3(OpCode::RecS, 3, 0, 2)); // 3
    module.emit(Instruction::new1(OpCode::Drop, 2)); // 4
    module.emit(Instruction::new0(OpCode::Halt)); // 5
    module.entry_point = Some(0);

    let mut vm = VM::new();
    vm.set_actor_callbacks(Box::new(RuntimeVmCallbacks::new(rt.clone())));
    vm.load_module(module);
    vm.run().unwrap();

    let rt_ref = rt.borrow();
    let actor = rt_ref.actors.get(&actor_id).unwrap();
    assert_eq!(
        actor.heap.live_count(),
        2,
        "object stored into a record field must survive a Drop of the original local (both the inner object and the record should remain live)"
    );
}

#[test]
fn test_vm_sconcat_uses_actor_heap() {
    use crate::bytecode::{CodeModule, Constant, Instruction, OpCode};
    use crate::runtime::heap::{ActorHeap, TypeTag};
    use crate::vm::VM;
    use std::cell::RefCell;
    use std::rc::Rc;

    let rt = Rc::new(RefCell::new(Runtime::new()));
    let actor_id = rt.borrow_mut().spawn_actor(Box::new(|| vec![]));
    rt.borrow_mut().current_actor = Some(actor_id);

    let mut module = CodeModule::new("test");
    let a_idx = module.add_constant(Constant::Int(12));
    let b_idx = module.add_constant(Constant::Int(34));
    module.emit(Instruction::new3(
        OpCode::ConstU,
        ((a_idx >> 8) & 0xFF) as u8,
        (a_idx & 0xFF) as u8,
        1,
    ));
    module.emit(Instruction::new3(
        OpCode::ConstU,
        ((b_idx >> 8) & 0xFF) as u8,
        (b_idx & 0xFF) as u8,
        2,
    ));
    module.emit(Instruction::new3(OpCode::SConcat, 1, 2, 0));
    module.emit(Instruction::new0(OpCode::Halt));
    module.entry_point = Some(0);

    let mut vm = VM::new();
    vm.set_actor_callbacks(Box::new(RuntimeVmCallbacks::new(rt.clone())));
    vm.load_module(module);
    vm.run().unwrap();

    let rt_ref = rt.borrow();
    let actor = rt_ref.actors.get(&actor_id).unwrap();
    assert_eq!(actor.heap.live_count(), 1);
    let mut ptrs = Vec::new();
    actor
        .heap
        .iter_live_objects(|_h, payload, _size| ptrs.push(payload));
    unsafe {
        let header = &*ActorHeap::header_of(ptrs[0]);
        assert_eq!(header.type_tag, TypeTag::String);
    }
}

/// v0.7 milestone: a persistent Counter survives 1,000 increments and a restart.
#[test]
fn test_persistent_counter_milestone_1000_messages() {
    let mut rt = Runtime::new();
    let mut models = HashMap::new();
    models.insert("count".to_string(), StateModel::Durable);
    let actor_id = rt.spawn_persistent_actor(
        Box::new(|| vec![("count".to_string(), Value::int(0))]),
        models,
    );
    rt.actors
        .get_mut(&actor_id)
        .unwrap()
        .register_behavior("inc", |actor, _args| {
            if let Some(n) = actor.get_state_field("count").and_then(|v| v.as_int()) {
                actor.set_state_field("count", Value::int(n + 1));
            }
        });

    for _ in 0..1000 {
        rt.send_message(actor_id, "inc", &[]);
    }
    rt.run_scheduler();

    assert_eq!(
        rt.actors
            .get(&actor_id)
            .unwrap()
            .get_state_field("count")
            .and_then(|v| v.as_int()),
        Some(1000)
    );

    // Simulate kill -9: drop the actor from memory but keep the store.
    rt.actors.remove(&actor_id);

    // Restart and recover.
    rt.recover_actor(actor_id).unwrap();

    // Re-register behavior handlers (they are code, not persisted state).
    rt.actors
        .get_mut(&actor_id)
        .unwrap()
        .register_behavior("inc", |actor, _args| {
            if let Some(n) = actor.get_state_field("count").and_then(|v| v.as_int()) {
                actor.set_state_field("count", Value::int(n + 1));
            }
        });

    // The recovered actor must have the durable state.
    assert_eq!(
        rt.actors
            .get(&actor_id)
            .unwrap()
            .get_state_field("count")
            .and_then(|v| v.as_int()),
        Some(1000)
    );

    // It should still be able to process new messages.
    rt.send_message(actor_id, "inc", &[]);
    rt.step_actor(actor_id);
    assert_eq!(
        rt.actors
            .get(&actor_id)
            .unwrap()
            .get_state_field("count")
            .and_then(|v| v.as_int()),
        Some(1001)
    );
}

/// Verify that the runtime restricts the cycle detector to local actors
/// and that the restriction is updated before each detection step.
#[test]
fn test_runtime_cycle_detector_intra_node_restriction() {
    let mut rt = Runtime::new();
    let a1 = rt.spawn_actor(Box::new(|| vec![("x".to_string(), Value::int(0))]));
    let a2 = rt.spawn_actor(Box::new(|| vec![("y".to_string(), Value::int(0))]));

    // Force enough detection epochs for the local-actor set to be applied.
    for _ in 0..15 {
        rt.process_gc_ops();
    }

    let local = rt.cycle_detector.local_actors();
    assert!(
        local.is_some(),
        "local-actor restriction should be set by Runtime"
    );
    let set = local.unwrap();
    assert!(set.contains(&a1), "actor a1 should be considered local");
    assert!(set.contains(&a2), "actor a2 should be considered local");
}

/// Verify that scheduler profiling counters are exposed through the Runtime.
#[test]
fn test_runtime_scheduler_stats() {
    let mut rt = Runtime::new();
    rt.reset_scheduler_stats();

    let a1 = rt.spawn_actor(Box::new(|| vec![("counter".to_string(), Value::int(0))]));
    let a2 = rt.spawn_actor(Box::new(|| vec![("counter".to_string(), Value::int(0))]));
    rt.send_message(a1, "add", &[Value::int(10)]);
    rt.send_message(a2, "add", &[Value::int(20)]);
    rt.run_scheduler();

    let stats = rt.scheduler_stats();
    assert_eq!(
        stats.total_tasks_processed, 4,
        "spawn + send should produce four actor tasks"
    );
    assert_eq!(
        stats.empty_polls, 1,
        "scheduler should poll empty once after draining"
    );

    rt.reset_scheduler_stats();
    let cleared = rt.scheduler_stats();
    assert_eq!(cleared.total_tasks_processed, 0);
    assert_eq!(cleared.empty_polls, 0);
}

// ========================================================================
// ORCA cycle-detector wiring tests
// ========================================================================

#[test]
fn test_cycle_detector_registers_real_cross_actor_ref() {
    let mut rt = Runtime::new();
    let a = rt.spawn_actor(Box::new(|| vec![]));
    let b = rt.spawn_actor(Box::new(|| vec![]));
    rt.current_actor = Some(a);

    let ptr = {
        let actor = rt.actors.get_mut(&a).unwrap();
        actor
            .heap
            .alloc(16, crate::runtime::heap::TypeTag::Raw)
            .unwrap()
    };
    unsafe {
        let header = &*ActorHeap::header_of(ptr);
        assert_eq!(
            header.actor_id, a,
            "heap actor_id should be set on creation"
        );
    }

    let v = Value::ptr(ptr);
    rt.send_message_by_id(b, 0, &[v]);
    assert_eq!(
        rt.cycle_detector.graph_size(),
        1,
        "cycle detector should track the foreign reference via the target actor sentinel"
    );

    rt.process_gc_ops();
    assert_eq!(
        rt.cycle_detector.graph_size(),
        0,
        "cycle detector should remove the edge after the op is processed"
    );
}

#[test]
fn test_cycle_detector_accumulates_edge_ref_count() {
    let mut rt = Runtime::new();
    let a = rt.spawn_actor(Box::new(|| vec![]));
    let b = rt.spawn_actor(Box::new(|| vec![]));
    rt.current_actor = Some(a);

    let ptr = rt
        .actors
        .get_mut(&a)
        .unwrap()
        .heap
        .alloc(16, crate::runtime::heap::TypeTag::Raw)
        .unwrap();
    let v = Value::ptr(ptr);

    rt.send_message_by_id(b, 0, &[v]);
    rt.send_message_by_id(b, 0, &[v]);
    assert_eq!(
        rt.cycle_detector.graph_size(),
        1,
        "only one sentinel node should exist for the target actor"
    );

    rt.process_gc_ops();
    // Both pending ops are drained in one call, so the edge ref_count drops
    // from 2 to 0 and the node is removed.
    assert_eq!(rt.cycle_detector.graph_size(), 0);
}

#[test]
fn test_cross_actor_send_foreign_count_lifecycle() {
    let mut rt = Runtime::new();
    let a = rt.spawn_actor(Box::new(|| vec![]));
    let b = rt.spawn_actor(Box::new(|| vec![]));
    rt.current_actor = Some(a);

    let ptr = rt
        .actors
        .get_mut(&a)
        .unwrap()
        .heap
        .alloc(16, TypeTag::Raw)
        .unwrap();
    unsafe {
        let header = &*ActorHeap::header_of(ptr);
        assert_eq!(header.ref_count, 1);
        assert_eq!(header.foreign_count, 0);
    }

    let v = Value::ptr(ptr);
    rt.send_message_by_id(b, 0, &[v]);

    unsafe {
        let header = &*ActorHeap::header_of(ptr);
        assert_eq!(header.ref_count, 1);
        assert_eq!(
            header.foreign_count, 1,
            "foreign_count should increment when ref is sent"
        );
    }

    rt.process_gc_ops();

    unsafe {
        let header = &*ActorHeap::header_of(ptr);
        assert_eq!(header.ref_count, 1);
        assert_eq!(
            header.foreign_count, 0,
            "foreign_count should decrement after op is processed on owning actor"
        );
    }

    // Drop the local ref on the owning actor; object should be freed.
    let actor = rt.actors.get_mut(&a).unwrap();
    unsafe {
        actor.orca_gc.drop_local_ref(&mut actor.heap, ptr);
    }
    assert_eq!(
        actor.heap.live_count(),
        0,
        "object should be freed after local+foreign counts hit zero"
    );
}

/// Regression test: the VM `Drop` callback must honor ORCA foreign counts.
/// An object another actor still references must be deferred, not freed.
#[test]
fn test_vm_drop_ref_defers_object_with_foreign_refs() {
    use crate::vm::ActorVmCallbacks;
    use std::cell::RefCell;
    use std::rc::Rc;

    let rt = Rc::new(RefCell::new(Runtime::new()));
    let actor_id = rt.borrow_mut().spawn_actor(Box::new(|| vec![]));
    rt.borrow_mut().current_actor = Some(actor_id);

    let mut cb = RuntimeVmCallbacks::new(rt.clone());
    let ptr = cb.alloc(16, TypeTag::Raw).unwrap();

    // Simulate an in-flight foreign reference held by another actor.
    unsafe {
        (*ActorHeap::header_of(ptr)).foreign_count = 1;
    }

    cb.drop_ref(ptr);
    assert_eq!(
        rt.borrow().actors.get(&actor_id).unwrap().heap.live_count(),
        1,
        "object with a live foreign reference must not be freed by Drop"
    );

    // Once the foreign reference goes away, the deferred pass reclaims it.
    unsafe {
        (*ActorHeap::header_of(ptr)).foreign_count = 0;
    }
    {
        let mut rt_mut = rt.borrow_mut();
        let actor = rt_mut.actors.get_mut(&actor_id).unwrap();
        actor.orca_gc.process_deferred(&mut actor.heap);
    }
    assert_eq!(
        rt.borrow().actors.get(&actor_id).unwrap().heap.live_count(),
        0,
        "deferred object should be freed once foreign_count returns to zero"
    );
}

/// Regression test: `run_scheduler` must pump the ORCA GC on its own.
/// A cross-actor reference whose local ref was dropped stays alive while
/// the receiver holds it and is reclaimed — without the embedder calling
/// `process_gc_ops` manually — once the receiver exits and releases its
/// hold.
#[test]
fn test_run_scheduler_pumps_gc() {
    let mut rt = Runtime::new();
    let a = rt.spawn_actor(Box::new(|| vec![]));
    let b = rt.spawn_actor(Box::new(|| vec![]));
    rt.current_actor = Some(a);

    let ptr = rt
        .actors
        .get_mut(&a)
        .unwrap()
        .heap
        .alloc(16, TypeTag::Raw)
        .unwrap();
    let v = Value::ptr(ptr);
    rt.send_message_by_id(b, 0, &[v]);

    // Sender drops its local reference while foreign_count is still 1: the
    // object must be deferred, not freed.
    {
        let actor = rt.actors.get_mut(&a).unwrap();
        unsafe {
            actor.orca_gc.drop_local_ref(&mut actor.heap, ptr);
        }
        assert_eq!(
            actor.heap.live_count(),
            1,
            "object should be deferred while foreign ref is live"
        );
    }

    // Draining the scheduler delivers the pending foreign-ref decrement
    // and retries deferred frees — no explicit process_gc_ops() call.  The
    // receiver popped the message, so it now holds the reference: the
    // object must survive until the receiver releases the hold.
    rt.run_scheduler();

    assert_eq!(
        rt.actors.get(&a).unwrap().heap.live_count(),
        1,
        "run_scheduler must not free an object the receiver still holds"
    );

    // The receiver exits: its hold is released and the scheduler's GC pump
    // reclaims the object.
    rt.exit_actor(b, ExitReason::Normal);
    rt.run_scheduler();

    assert_eq!(
        rt.actors.get(&a).unwrap().heap.live_count(),
        0,
        "object should be reclaimed once the receiver releases its hold"
    );
}

/// Regression test (ORCA memory safety): a sender that exits with a
/// foreign-ref op still pending must not leave `process_gc_ops` reading
/// freed heap memory.  The op carries the owner id (no header deref), and
/// the exiting actor's heap is retired while foreign refs are outstanding,
/// then reclaimed once they drain.
#[test]
fn test_exiting_sender_heap_retired_until_refs_drain() {
    let mut rt = Runtime::new();
    let a = rt.spawn_actor(Box::new(|| vec![]));
    let b = rt.spawn_actor(Box::new(|| vec![]));
    rt.current_actor = Some(a);

    let ptr = rt
        .actors
        .get_mut(&a)
        .unwrap()
        .heap
        .alloc(16, TypeTag::Raw)
        .unwrap();
    let v = Value::ptr(ptr);
    rt.send_message_by_id(b, 0, &[v]);

    // A exits with the in-flight op still pending and B's message unread.
    rt.exit_actor(a, ExitReason::Normal);
    assert!(
        !rt.actors.contains_key(&a),
        "exited actor should be removed from the map"
    );
    assert_eq!(
        rt.retired_heaps.len(),
        1,
        "heap with an outstanding foreign ref must be retired, not freed"
    );

    // B receives the pointer (taking a hold), then the scheduler drains:
    // process_gc_ops applies the pending -1 against the retired heap —
    // before the fix this dereferenced freed heap memory.
    rt.run_scheduler();

    // B's hold keeps the heap retired: the header is still readable with
    // foreign_count >= 1.
    unsafe {
        let header = &*ActorHeap::header_of(ptr);
        assert!(
            header.foreign_count >= 1,
            "receiver hold must keep the retired heap object alive"
        );
    }

    // Once B exits, its hold is released and the retired heap is reclaimed.
    rt.exit_actor(b, ExitReason::Normal);
    assert!(
        rt.retired_heaps.is_empty(),
        "retired heap should be reclaimed once all foreign refs drain"
    );
}

/// Regression test: forwarding a received heap reference must use the
/// true owner recorded in the object header, not the forwarding actor —
/// the old code tripped the `send_ref_to` ownership debug_assert and, in
/// release builds, registered the cycle-detector edge under the wrong
/// actor.
#[test]
fn test_forwarding_received_reference_uses_true_owner() {
    let mut rt = Runtime::new();
    let a = rt.spawn_actor(Box::new(|| vec![]));
    let b = rt.spawn_actor(Box::new(|| vec![]));
    let c = rt.spawn_actor(Box::new(|| vec![]));

    let ptr = rt
        .actors
        .get_mut(&a)
        .unwrap()
        .heap
        .alloc(16, TypeTag::Raw)
        .unwrap();
    let v = Value::ptr(ptr);

    // A sends the reference to B; B receives it (taking a hold).
    rt.current_actor = Some(a);
    rt.send_message_by_id(b, 0, &[v]);
    rt.run_scheduler();

    // B forwards the reference to C.  Before the fix this panicked in
    // debug builds (the object is owned by A, not B).
    rt.current_actor = Some(b);
    rt.send_message_by_id(c, 0, &[v]);

    // The foreign count lives on A's object: B's hold plus the in-flight
    // forward must both be counted there.
    unsafe {
        let header = &*ActorHeap::header_of(ptr);
        assert_eq!(header.actor_id, a, "object is owned by A");
        assert!(
            header.foreign_count >= 2,
            "hold + in-flight forward should both be counted, got {}",
            header.foreign_count
        );
    }

    // The cycle-detector edge must be registered under the true owner A
    // (target C's sentinel -> A's object), not under B.
    assert_eq!(
        rt.cycle_detector.graph_size(),
        1,
        "forwarded reference should register exactly one edge"
    );

    // Draining delivers the forward's -1 to A's heap; B's hold still keeps
    // the object alive afterwards.
    rt.run_scheduler();
    unsafe {
        let header = &*ActorHeap::header_of(ptr);
        assert!(
            header.foreign_count >= 1,
            "B's hold must keep the object alive after the forward lands"
        );
    }
}

/// Regression test: an object whose pointer was received by another actor
/// must survive the sender dropping all of its local references, and be
/// reclaimed only when the receiver releases its hold (here: on exit).
#[test]
fn test_receiver_hold_survives_sender_drop_until_release() {
    let mut rt = Runtime::new();
    let a = rt.spawn_actor(Box::new(|| vec![]));
    let b = rt.spawn_actor(Box::new(|| vec![]));
    rt.current_actor = Some(a);

    let ptr = rt
        .actors
        .get_mut(&a)
        .unwrap()
        .heap
        .alloc(16, TypeTag::Raw)
        .unwrap();
    let v = Value::ptr(ptr);
    rt.send_message_by_id(b, 0, &[v]);

    // B receives the message and holds the reference.
    rt.run_scheduler();

    // A drops its last local reference.  Before the fix the object was
    // freed here even though B still holds the pointer.
    {
        let actor = rt.actors.get_mut(&a).unwrap();
        unsafe {
            actor.orca_gc.drop_local_ref(&mut actor.heap, ptr);
        }
        assert_eq!(
            actor.heap.live_count(),
            1,
            "object must survive while the receiver holds it"
        );
    }

    // B exits: the hold is released and the object is freed on A's heap.
    rt.exit_actor(b, ExitReason::Normal);
    assert_eq!(
        rt.actors.get(&a).unwrap().heap.live_count(),
        0,
        "object should be freed once the receiver releases its hold"
    );
}

// ========================================================================
// v0.8 Workflow Runtime Tests
// ========================================================================

#[test]
fn test_workflow_actor_emits_started_event() {
    let mut rt = Runtime::new();
    let mut models = HashMap::new();
    models.insert("step_index".to_string(), StateModel::Durable);
    let actor_id = rt.spawn_workflow_actor(
        "CounterWorkflow",
        Box::new(|| vec![("step_index".to_string(), Value::int(0))]),
        models,
    );

    let events = rt.persistence.read_workflow_events(actor_id);
    assert_eq!(events.len(), 1);
    assert!(
        matches!(&events[0], WorkflowEvent::WorkflowStarted { name, .. } if name == "CounterWorkflow")
    );

    let snapshot = rt.persistence.load_snapshot(actor_id).unwrap();
    assert_eq!(
        snapshot.state.get("step_index"),
        Some(&PersistedValue::Int(0))
    );
}

#[test]
fn test_workflow_actor_step_event_and_checkpoint() {
    let mut rt = Runtime::new();
    let mut models = HashMap::new();
    models.insert("step_index".to_string(), StateModel::Durable);
    let actor_id = rt.spawn_workflow_actor(
        "CounterWorkflow",
        Box::new(|| vec![("step_index".to_string(), Value::int(0))]),
        models,
    );

    rt.actors
        .get_mut(&actor_id)
        .unwrap()
        .register_behavior("next", |actor, _args| {
            if let Some(n) = actor.get_state_field("step_index").and_then(|v| v.as_int()) {
                actor.set_state_field("step_index", Value::int(n + 1));
            }
        });

    rt.send_message(actor_id, "next", &[]);
    rt.step_actor(actor_id);

    let events = rt.persistence.read_workflow_events(actor_id);
    assert_eq!(events.len(), 2);
    assert!(matches!(&events[1], WorkflowEvent::StepCompleted { .. }));

    let snapshot = rt.persistence.load_snapshot(actor_id).unwrap();
    assert_eq!(
        snapshot.state.get("step_index"),
        Some(&PersistedValue::Int(1))
    );
}

#[test]
fn test_workflow_actor_recovery_replays_step_index() {
    let mut rt = Runtime::new();
    let mut models = HashMap::new();
    models.insert("step_index".to_string(), StateModel::Durable);
    let actor_id = rt.spawn_workflow_actor(
        "CounterWorkflow",
        Box::new(|| vec![("step_index".to_string(), Value::int(0))]),
        models,
    );

    rt.actors
        .get_mut(&actor_id)
        .unwrap()
        .register_behavior("next", |actor, _args| {
            if let Some(n) = actor.get_state_field("step_index").and_then(|v| v.as_int()) {
                actor.set_state_field("step_index", Value::int(n + 1));
            }
        });

    for _ in 0..3 {
        rt.send_message(actor_id, "next", &[]);
        rt.step_actor(actor_id);
    }

    // Simulate node restart: drop the actor from memory but keep the store.
    rt.actors.remove(&actor_id);

    rt.recover_actor(actor_id).unwrap();
    rt.actors
        .get_mut(&actor_id)
        .unwrap()
        .register_behavior("next", |actor, _args| {
            if let Some(n) = actor.get_state_field("step_index").and_then(|v| v.as_int()) {
                actor.set_state_field("step_index", Value::int(n + 1));
            }
        });

    let step_index = rt
        .actors
        .get(&actor_id)
        .unwrap()
        .get_state_field("step_index")
        .and_then(|v| v.as_int())
        .unwrap();
    assert_eq!(step_index, 3);

    // The actor should still be able to advance.
    rt.send_message(actor_id, "next", &[]);
    rt.step_actor(actor_id);
    let step_index = rt
        .actors
        .get(&actor_id)
        .unwrap()
        .get_state_field("step_index")
        .and_then(|v| v.as_int())
        .unwrap();
    assert_eq!(step_index, 4);
}

// ---------------------------------------------------------------------------
// Workflow event journal foundation tests (timer / signal / saga)
// ---------------------------------------------------------------------------

#[test]
fn test_memory_store_append_read_timer_events() {
    let mut store = MemoryStore::new();
    store.append_timer_set(1, 1, "t1".to_string(), 100).unwrap();
    store.append_timer_fired(1, 2, "t1".to_string()).unwrap();

    let timers = store.read_timer_events(1);
    assert_eq!(timers.len(), 2);
    assert!(
        matches!(&timers[0], WorkflowEvent::TimerSet { name, duration_ms, .. } if name == "t1" && *duration_ms == 100)
    );
    assert!(matches!(&timers[1], WorkflowEvent::TimerFired { name, .. } if name == "t1"));
}

#[test]
fn test_memory_store_append_read_signal_event() {
    let mut store = MemoryStore::new();
    store
        .append_signal_received(1, 1, "resume".to_string(), Some("go".to_string()))
        .unwrap();

    let signals = store.read_signal_events(1);
    assert_eq!(signals.len(), 1);
    assert!(
        matches!(&signals[0], WorkflowEvent::SignalReceived { name, payload, .. } if name == "resume" && payload == &Some("go".to_string()))
    );
}

#[test]
fn test_memory_store_append_read_saga_event() {
    let mut store = MemoryStore::new();
    store
        .append_saga_compensated(1, 1, "charge_card".to_string())
        .unwrap();

    let sagas = store.read_saga_events(1);
    assert_eq!(sagas.len(), 1);
    assert!(
        matches!(&sagas[0], WorkflowEvent::SagaCompensated { step_name, .. } if step_name == "charge_card")
    );
}

#[cfg(feature = "sqlite")]
#[test]
fn test_libsql_store_append_read_new_workflow_events() {
    let mut store = LibsqlStore::in_memory().unwrap();
    store.append_timer_set(1, 1, "t1".to_string(), 200).unwrap();
    store
        .append_signal_received(1, 2, "cancel".to_string(), None)
        .unwrap();
    store
        .append_saga_compensated(1, 3, "reserve".to_string())
        .unwrap();

    let all = store.read_workflow_events(1);
    assert_eq!(all.len(), 3);
    assert!(matches!(&all[0], WorkflowEvent::TimerSet { .. }));
    assert!(matches!(&all[1], WorkflowEvent::SignalReceived { .. }));
    assert!(matches!(&all[2], WorkflowEvent::SagaCompensated { .. }));

    assert_eq!(store.read_timer_events(1).len(), 1);
    assert_eq!(store.read_signal_events(1).len(), 1);
    assert_eq!(store.read_saga_events(1).len(), 1);
    assert_eq!(store.latest_sequence(1), 3);
}

#[test]
fn test_runtime_append_workflow_timer_signal_saga_events() {
    let mut rt = Runtime::new();
    let mut models = HashMap::new();
    models.insert("step_index".to_string(), StateModel::Durable);
    let actor_id = rt.spawn_workflow_actor(
        "OrderWorkflow",
        Box::new(|| vec![("step_index".to_string(), Value::int(0))]),
        models,
    );

    rt.append_timer_set(actor_id, "payment_timeout", 5000)
        .unwrap();
    rt.append_timer_fired(actor_id, "payment_timeout").unwrap();
    rt.append_signal_received(actor_id, "cancel", Some("user_123".to_string()))
        .unwrap();
    rt.append_saga_compensated(actor_id, "authorize_payment")
        .unwrap();

    let events = rt.persistence.read_workflow_events(actor_id);
    assert_eq!(events.len(), 5); // WorkflowStarted + 4 new events
    assert!(
        matches!(&events[1], WorkflowEvent::TimerSet { name, duration_ms, .. } if name == "payment_timeout" && *duration_ms == 5000)
    );
    assert!(
        matches!(&events[2], WorkflowEvent::TimerFired { name, .. } if name == "payment_timeout")
    );
    assert!(
        matches!(&events[3], WorkflowEvent::SignalReceived { name, payload, .. } if name == "cancel" && payload == &Some("user_123".to_string()))
    );
    assert!(
        matches!(&events[4], WorkflowEvent::SagaCompensated { step_name, .. } if step_name == "authorize_payment")
    );
}

#[test]
fn test_workflow_recovery_handles_new_event_variants() {
    let mut rt = Runtime::new();
    let mut models = HashMap::new();
    models.insert("step_index".to_string(), StateModel::Durable);
    let actor_id = rt.spawn_workflow_actor(
        "OrderWorkflow",
        Box::new(|| vec![("step_index".to_string(), Value::int(0))]),
        models,
    );

    rt.append_timer_set(actor_id, "t1", 100).unwrap();
    rt.append_signal_received(actor_id, "s1", Some("payload".to_string()))
        .unwrap();
    rt.append_saga_compensated(actor_id, "step_a").unwrap();

    rt.actors.remove(&actor_id);
    rt.recover_actor(actor_id).unwrap();

    let step_index = rt
        .actors
        .get(&actor_id)
        .unwrap()
        .get_state_field("step_index")
        .and_then(|v| v.as_int())
        .unwrap();
    assert_eq!(step_index, 0);

    let events = rt.persistence.read_workflow_events(actor_id);
    assert_eq!(events.len(), 4);
}

// ---------------------------------------------------------------------------
// Pipeline Tests
// ---------------------------------------------------------------------------

#[cfg(feature = "ai-runtime")]
#[test]
fn test_pipeline_runtime_api() {
    use nulang_ai::PipelineRuntime;

    let mut rt = Runtime::new();

    // Create a pipeline through the runtime API.
    let id = rt.pipeline_new();
    assert!(rt.ai.pipelines.contains_key(&id));

    // Add a stage.
    let result = rt.pipeline_stage(id, "summarize", 42, "Summarize: {input}");
    assert_eq!(result, Ok(id));
    assert_eq!(rt.ai.pipelines[&id].stages.len(), 1);
    assert_eq!(rt.ai.pipelines[&id].stages[0].name, "summarize");
    assert_eq!(rt.ai.pipelines[&id].stages[0].agent_id, 42);
    assert_eq!(
        rt.ai.pipelines[&id].stages[0].prompt_template,
        "Summarize: {input}"
    );

    // Run the stored pipeline against a mock runtime to avoid spinning up
    // real actors/LLM clients in this unit test.
    struct MockRuntime;
    impl PipelineRuntime for MockRuntime {
        fn ask_agent(&mut self, agent_id: u64, prompt: &str) -> Result<String, String> {
            Ok(format!("agent {} got {}", agent_id, prompt))
        }
    }
    let pipeline = rt.ai.pipelines[&id].clone();
    let output = pipeline.run(&mut MockRuntime, "hello world").unwrap();
    assert_eq!(output, "agent 42 got Summarize: hello world");
}

// ---------------------------------------------------------------------------
// Multi-Node Distributed Tests
// ---------------------------------------------------------------------------

use rcgen::{BasicConstraints, CertificateParams, IsCa, Issuer, KeyPair, KeyUsagePurpose};
use std::net::{IpAddr, Ipv4Addr, SocketAddr};
use std::thread::sleep;

/// Shared CertificateParams for the test CA — used by both generate_test_ca
/// and generate_test_leaf so leaf certificates are correctly signed without
/// needing the `x509-parser` feature to re-parse PEM.
fn ca_cert_params() -> CertificateParams {
    let mut params = CertificateParams::new(vec![]).expect("ca params");
    params.is_ca = IsCa::Ca(BasicConstraints::Unconstrained);
    params.key_usages = vec![KeyUsagePurpose::KeyCertSign, KeyUsagePurpose::CrlSign];
    params
}

/// Generate a self-signed CA certificate and key (PEM-encoded).
fn generate_test_ca() -> (Vec<u8>, KeyPair) {
    let params = ca_cert_params();
    let key = KeyPair::generate().expect("key gen");
    let cert = params.self_signed(&key).expect("ca gen");
    (cert.pem().into_bytes(), key)
}

/// Generate a leaf certificate signed by the CA, for the given node name.
/// Returns (cert_pem, key_pem).
///
/// The `ca_cert_pem` parameter is accepted for API clarity but not consumed —
/// the CA params needed for signing are reconstructed from `ca_cert_params()`
/// because rcgen's `Issuer::from_ca_cert_pem` requires the optional
/// `x509-parser` feature.
fn generate_test_leaf(name: &str, ca_key: &KeyPair, _ca_cert_pem: &[u8]) -> (Vec<u8>, Vec<u8>) {
    let ca_params = ca_cert_params();
    let params = CertificateParams::new(vec![name.to_string(), "localhost".to_string()])
        .expect("leaf params");
    let key = KeyPair::generate().expect("leaf key gen");
    let issuer = Issuer::from_params(&ca_params, ca_key);
    let cert = params.signed_by(&key, &issuer).expect("leaf gen");
    (cert.pem().into_bytes(), key.serialize_pem().into_bytes())
}

#[cfg(feature = "tcp")]
/// Start a distributed runtime with MutualTLS enabled, bound to an ephemeral port.
fn start_mutual_tls_node(ca_cert_pem: &[u8], cert_pem: &[u8], key_pem: &[u8]) -> Runtime {
    let mut rt = Runtime::new();
    rt.enable_distribution(
        SocketAddr::new(IpAddr::V4(Ipv4Addr::new(127, 0, 0, 1)), 0),
        crate::runtime::network::TlsConfig::MutualTls {
            ca_cert_pem: ca_cert_pem.to_vec(),
            server_cert_pem: cert_pem.to_vec(),
            server_key_pem: key_pem.to_vec(),
            server_name: Some("localhost".to_string()),
        },
    )
    .expect("failed to enable distribution with MutualTls");
    rt
}

#[cfg(feature = "tcp")]
/// Start a distributed-enabled runtime bound to an ephemeral loopback port.
fn start_distributed_node() -> Runtime {
    let mut rt = Runtime::new();
    rt.enable_distribution(
        SocketAddr::new(IpAddr::V4(Ipv4Addr::new(127, 0, 0, 1)), 0),
        crate::runtime::network::TlsConfig::PlaintextInsecure,
    )
    .expect("failed to enable distribution");
    rt
}

#[cfg(feature = "tcp")]
/// Node-death recovery (PLAN.md Phase 5 deliverable 7, parts a+b): when a
/// peer node is declared `Failed`, the local runtime must (a) invalidate
/// its `RemoteActorCache` entries so sends fail fast instead of
/// stale-resolving to a dead node, and (b) deliver a
/// `DOWN`-with-`noconnection` system message to every local actor that had
/// linked or monitored an actor on the failed node, dropping the now-dead
/// registry entries. (Part c — supervisor-policy re-spawn of durable
/// actors — is deliberately out of scope: it needs the
/// old-node-confirmed-gone gate that a split-brain resolver decision, not
/// a bare failure-detection signal, provides.)
#[test]
fn test_node_failed_invalidates_cache_and_delivers_down() {
    use crate::runtime::supervision::RemoteLink;
    let mut rt = start_distributed_node();
    let local_node = rt.distributed.node_id.unwrap_or(NodeId::LOCAL);
    let dead_node = NodeId(4242);

    // (a) Seed the remote cache with entries on the doomed node.
    rt.distributed
        .resolver
        .as_mut()
        .unwrap()
        .record_remote_send(dead_node, 100);
    rt.distributed
        .resolver
        .as_mut()
        .unwrap()
        .record_remote_send(dead_node, 101);
    rt.distributed
        .resolver
        .as_mut()
        .unwrap()
        .record_remote_send(NodeId(9999), 200);

    // (b) A local actor monitors (and a second one links to) a remote actor
    // on the doomed node.
    let monitor_watcher = rt.spawn_actor(Box::new(|| vec![]));
    let link_watcher = rt.spawn_actor(Box::new(|| vec![]));
    let remote_target = RemoteLink {
        node_id: dead_node,
        actor_id: 100,
    };
    rt.remote_monitors.register(
        remote_target,
        RemoteLink {
            node_id: local_node,
            actor_id: monitor_watcher,
        },
    );
    rt.remote_links.register(
        remote_target,
        RemoteLink {
            node_id: local_node,
            actor_id: link_watcher,
        },
    );

    crate::runtime::distribution::handle_node_failed(&mut rt, dead_node);

    // (a) Cache entries for the failed node are gone; unrelated entries live.
    let cache = rt.distributed.resolver.as_mut().unwrap().cache_mut();
    assert_eq!(cache.len(), 1, "only the unrelated node's entry survives");
    assert!(cache.get(dead_node, 100).is_none());
    assert!(cache.get(dead_node, 101).is_none());

    // (b) Both watchers received a DOWN system message (behavior 0) with
    // the noconnection code (6) and the dead target's actor id.
    for watcher_id in [monitor_watcher, link_watcher] {
        let down = rt
            .actors
            .get_mut(&watcher_id)
            .unwrap()
            .mailbox
            .pop()
            .expect("watcher must receive a DOWN on node failure");
        assert_eq!(down.behavior_id, 0, "DOWN is a system message");
        assert_eq!(down.payload[0].as_int(), Some(100), "target actor id");
        assert_eq!(down.payload[1].as_int(), Some(watcher_id as i64));
        assert_eq!(down.payload[2].as_int(), Some(6), "noconnection code");
    }

    // The registry entries for the dead node are dropped.
    assert!(
        rt.remote_monitors.get_watchers(remote_target).is_none(),
        "monitor registry entry for the dead node must be dropped"
    );
    assert!(
        rt.remote_links.get_watchers(remote_target).is_none(),
        "link registry entry for the dead node must be dropped"
    );
}

/// Pump `process_network` on every node until each node's cluster view
/// holds `expected` healthy members (including itself), or fail.
///
/// Every poll iteration pumps ALL nodes (each `process_network` also runs
/// the cluster `tick`, which drives heartbeats, gossip, and membership
/// timeouts), then sleeps a fixed 50 ms — no assumption is made about
/// wall-clock ordering between nodes. Callers should pass a generous
/// deadline (30 s): convergence is normally sub-second, but under heavy
/// CPU load the real-TCP handshake and heartbeat cadence can degrade by
/// an order of magnitude.
fn pump_until_converged(nodes: &mut [&mut Runtime], expected: usize, timeout: Duration) {
    let deadline = Instant::now() + timeout;
    loop {
        let mut counts = Vec::new();
        for rt in nodes.iter_mut() {
            rt.process_network();
            let count = rt
                .distributed
                .cluster
                .as_ref()
                .unwrap()
                .healthy_node_count();
            counts.push(count);
        }
        if counts.iter().all(|&c| c == expected) {
            return;
        }
        assert!(
            Instant::now() < deadline,
            "cluster did not converge to {} healthy nodes (counts: {:?})",
            expected,
            counts
        );
        sleep(Duration::from_millis(50));
    }
}

/// Shut down the transports of the given nodes.
fn shutdown_nodes(nodes: &mut [&mut Runtime]) {
    for rt in nodes.iter_mut() {
        if let Some(mut transport) = rt.distributed.transport.take() {
            transport.shutdown();
        }
    }
}

#[cfg(feature = "tcp")]
#[test]
fn test_three_node_cluster_membership_converges() {
    let mut rt_a = start_distributed_node();
    let mut rt_b = start_distributed_node();
    let mut rt_c = start_distributed_node();

    let addr_a = rt_a.distributed.transport.as_ref().unwrap().listen_addr();
    let addr_b = rt_b.distributed.transport.as_ref().unwrap().listen_addr();
    let node_a = rt_a.distributed.node_id.unwrap();
    let node_b = rt_b.distributed.node_id.unwrap();
    let node_c = rt_c.distributed.node_id.unwrap();

    // The local node's own cluster entry must carry the real listen
    // address, not the port-0 bind address.
    assert_eq!(
        rt_a.distributed
            .cluster
            .as_ref()
            .unwrap()
            .get_node(node_a)
            .unwrap()
            .address,
        addr_a
    );

    // Full-mesh join: each new node seeds from every existing node.
    // (Transitive gossip propagation over the wire is covered separately
    // by test_three_node_gossip_converges_chain_seeded; pairwise seeding
    // plus heartbeat-based discovery converges the mesh here regardless.)
    rt_b.join_cluster(addr_a);
    rt_c.join_cluster(addr_a);
    rt_c.join_cluster(addr_b);

    pump_until_converged(
        &mut [&mut rt_a, &mut rt_b, &mut rt_c],
        3,
        Duration::from_secs(30),
    );

    // Every node sees every other node as a healthy member.
    for rt in [&rt_a, &rt_b, &rt_c] {
        let cluster = rt.distributed.cluster.as_ref().unwrap();
        for peer in [node_a, node_b, node_c] {
            let info = cluster
                .get_node(peer)
                .expect("peer missing from membership table");
            assert_eq!(
                info.status,
                NodeStatus::Healthy,
                "peer {:?} not healthy",
                peer
            );
        }
    }

    // Addresses learned by seeding carry the peer's real listen address.
    assert_eq!(
        rt_b.distributed
            .cluster
            .as_ref()
            .unwrap()
            .get_node(node_a)
            .unwrap()
            .address,
        addr_a
    );
    assert_eq!(
        rt_c.distributed
            .cluster
            .as_ref()
            .unwrap()
            .get_node(node_b)
            .unwrap()
            .address,
        addr_b
    );

    shutdown_nodes(&mut [&mut rt_a, &mut rt_b, &mut rt_c]);
}

#[cfg(feature = "tcp")]
#[test]
fn test_three_node_remote_actor_message_delivery() {
    let mut rt_a = start_distributed_node();
    let mut rt_b = start_distributed_node();
    let mut rt_c = start_distributed_node();

    let addr_a = rt_a.distributed.transport.as_ref().unwrap().listen_addr();
    let addr_b = rt_b.distributed.transport.as_ref().unwrap().listen_addr();
    let node_a = rt_a.distributed.node_id.unwrap();

    rt_b.join_cluster(addr_a);
    rt_c.join_cluster(addr_a);
    rt_c.join_cluster(addr_b);
    pump_until_converged(
        &mut [&mut rt_a, &mut rt_b, &mut rt_c],
        3,
        Duration::from_secs(30),
    );

    // An actor on node A with a decoy behavior (table index 0) and the
    // intended behavior (index 1). Remote packets carry the behavior
    // *name* and the receiver resolves it against the target actor's
    // behavior table (see process_network_packets), so dispatch must run
    // "store" — if it ever fell back to index-based dispatch the decoy
    // would run and fail this test.
    let actor_id = rt_a.spawn_actor(Box::new(|| vec![("received".to_string(), Value::int(0))]));
    {
        let actor = rt_a.actors.get_mut(&actor_id).unwrap();
        actor.register_behavior("decoy", |actor, _args| {
            actor.set_state_field("received", Value::int(-999));
        });
        actor.register_behavior("store", |actor, args| {
            let n = args.get(0).and_then(|v| v.as_int()).unwrap_or(-1);
            actor.set_state_field("received", Value::int(n));
        });
    }

    // Node C sends to the actor on node A through the location-transparent
    // address (remote node + actor id), with node B present in the mesh.
    let target = ActorAddress::remote(node_a, actor_id);
    rt_c.send_distributed(target, "store", &[Value::int(42)]);

    // Generous deadline for loaded machines; every iteration pumps ALL
    // nodes so heartbeats keep flowing and no membership view degrades
    // (suspicion kicks in after 2 s of silence) while we wait.
    let deadline = Instant::now() + Duration::from_secs(30);
    let delivered = loop {
        rt_a.process_network();
        rt_b.process_network();
        rt_c.process_network();
        rt_a.run_scheduler();
        let got = rt_a
            .actors
            .get(&actor_id)
            .and_then(|a| a.get_state_field("received"))
            .and_then(|v| v.as_int());
        if got == Some(42) {
            break true;
        }
        assert_ne!(
            got,
            Some(-999),
            "decoy behavior dispatched for the remote message"
        );
        if Instant::now() >= deadline {
            break false;
        }
        sleep(Duration::from_millis(50));
    };
    assert!(
        delivered,
        "remote message from node C was not delivered to the actor on node A"
    );

    shutdown_nodes(&mut [&mut rt_a, &mut rt_b, &mut rt_c]);
}

#[cfg(feature = "tcp")]
#[test]
fn test_actor_migration_between_two_nodes() {
    use crate::bytecode::{ActorMeta, BehaviorTableEntry, CodeModule, Constant};
    use crate::runtime::persistence::ActorSnapshot;
    use std::thread::sleep;
    use std::time::Duration;

    let mut rt_a = start_distributed_node();
    let mut rt_b = start_distributed_node();

    let addr_a = rt_a.distributed.transport.as_ref().unwrap().listen_addr();
    let node_b = rt_b.distributed.node_id.unwrap();

    rt_b.join_cluster(addr_a);
    pump_until_converged(&mut [&mut rt_a, &mut rt_b], 2, Duration::from_secs(30));

    // Build a persistent actor module with a "store" behavior.
    let mut module = CodeModule::new("test_migration");
    module.add_actor_meta(ActorMeta {
        name: "Counter".to_string(),
        persistent: true,
        state_models: vec![("count".to_string(), crate::ast::StateModel::Durable)],
        state_defaults: vec![("count".to_string(), Constant::Int(0))],
        behavior_indices: vec![0],
        type_hash: None,
        version: 1,
        migrations: String::new(),
        is_workflow: false,
        is_agent: false,
        is_organization: false,
        is_virtual: false,
        tools: vec![],
        semantic_memory_dimensions: None,
        procedural_memory_namespace: None,
        backend: crate::ast::ActorBackendKind::Native,
        fallback_config: String::new(),
        retry_config: String::new(),
    });
    module.add_behavior(BehaviorTableEntry {
        name: "store".to_string(),
        param_count: 1,
        code_offset: 0,
        local_count: 1,
        effect_mask: 0,
        compensate_offset: None,
        content_hash: None,
        source_location: None,
        parallel_branches: None,
    });

    // Spawn the actor on node A.
    let actor_id = rt_a.spawn_actor(Box::new(|| vec![("count".to_string(), Value::int(0))]));
    {
        let actor = rt_a.actors.get_mut(&actor_id).unwrap();
        actor.persistent = true;
        actor.register_behavior("store", |actor, args| {
            let n = args.get(0).and_then(|v| v.as_int()).unwrap_or(-1);
            actor.set_state_field("count", Value::int(n));
        });
    }
    // Register the recovery module so migration can find the bytecode.
    let offsets: Vec<usize> = module
        .behaviors
        .iter()
        .map(|b| b.code_offset as usize)
        .collect();
    let comp_offsets: Vec<Option<usize>> = module
        .behaviors
        .iter()
        .map(|b| b.compensate_offset.map(|o| o as usize))
        .collect();
    crate::runtime::spawn::register_recovery_module(
        &mut rt_a,
        actor_id,
        module.clone(),
        offsets.clone(),
        comp_offsets.clone(),
    );

    // Build the migration payload manually (same logic as the callback).
    let (snapshot_json, nbc_bytes) = {
        let actor = rt_a.actors.get(&actor_id).unwrap();
        let mut state = std::collections::HashMap::new();
        for (name, value) in &actor.state_data {
            let model = actor
                .state_models
                .get(name)
                .copied()
                .unwrap_or(crate::runtime::persistence::StateModel::Local);
            if model == crate::runtime::persistence::StateModel::Durable || model.is_crdt() {
                let persisted = crate::runtime::persistence::PersistedValue::from_value_resolved(
                    value,
                    actor.bytecode_module.as_ref(),
                );
                state.insert(name.clone(), persisted);
            }
        }
        let crdt_snapshot = rt_a.crdt_manager.as_ref().map(|m| {
            m.snapshot()
                .into_iter()
                .map(|(id, (ty, bytes))| (id.0, ty.to_u8(), bytes))
                .collect()
        });
        let snapshot = ActorSnapshot {
            actor_id,
            sequence: actor.sequence,
            state,
            waiting_signal: actor.waiting_signal.clone(),
            crdt_snapshot,
        };
        let json = serde_json::to_vec(&snapshot).unwrap();
        let nbc = module.to_nbc(None).unwrap();
        (json, nbc)
    }; // actor borrow released

    // Send the migration packet from A to B.
    let packet = crate::runtime::network::Packet::MigrateActor {
        actor_id,
        nbc_bytes,
        snapshot_json,
    };
    let addr_b = rt_b.distributed.transport.as_ref().unwrap().listen_addr();
    rt_a.distributed
        .transport
        .as_mut()
        .unwrap()
        .send(node_b, addr_b, packet);

    // Pump network so B receives the MigrateActor packet.
    let deadline = Instant::now() + Duration::from_secs(10);
    let received = loop {
        rt_a.process_network();
        rt_b.process_network();
        if rt_b.actors.contains_key(&actor_id) {
            break true;
        }
        if Instant::now() >= deadline {
            break false;
        }
        sleep(Duration::from_millis(10));
    };
    assert!(received, "actor {} was not received on node B", actor_id);

    // Register the native behavior handler on the target node (native
    // handlers are not serialized in the migration payload — only the
    // bytecode module and durable state are).
    {
        let actor = rt_b.actors.get_mut(&actor_id).unwrap();
        assert!(actor.persistent);
        assert_eq!(
            actor.get_state_field("count"),
            Some(Value::int(0)),
            "migrated actor should have its durable state intact"
        );
        actor.register_behavior("store", |actor, args| {
            let n = args.get(0).and_then(|v| v.as_int()).unwrap_or(-1);
            actor.set_state_field("count", Value::int(n));
        });
    }

    // Send a message to the actor on B and verify it processes.
    rt_b.send_message(actor_id, "store", &[Value::int(99)]);
    rt_b.run_scheduler();
    assert_eq!(
        rt_b.actors
            .get(&actor_id)
            .and_then(|a| a.get_state_field("count"))
            .and_then(|v| v.as_int()),
        Some(99),
        "migrated actor should process messages on the target node"
    );

    // Register forwarding on A and verify messages are forwarded to B.
    rt_a.migrated_actors
        .insert(actor_id, (node_b, Instant::now()));
    // The message should be forwarded from A to B via send_distributed.
    // Send through send_message which calls send_message_by_id which
    // checks migrated_actors. The behavior name is resolved from the
    // recovery module.
    rt_a.send_message(actor_id, "store", &[Value::int(42)]);

    // Pump to deliver the forwarded message.
    let deadline = Instant::now() + Duration::from_secs(10);
    let forwarded = loop {
        rt_a.process_network();
        rt_b.process_network();
        rt_b.run_scheduler();
        let got = rt_b
            .actors
            .get(&actor_id)
            .and_then(|a| a.get_state_field("count"))
            .and_then(|v| v.as_int());
        if got == Some(42) {
            break true;
        }
        if Instant::now() >= deadline {
            break false;
        }
        sleep(Duration::from_millis(10));
    };
    assert!(
        forwarded,
        "forwarded message should arrive on the migrated actor on node B"
    );

    shutdown_nodes(&mut [&mut rt_a, &mut rt_b]);
}

/// Pump network processing on the surviving `nodes` until `dead` is
/// marked `NodeStatus::Failed` in every survivor's cluster view, or the
/// deadline elapses. Mirrors `pump_until_converged`'s polling shape;
/// real wall-clock time must actually pass here (heartbeat timeout +
/// suspicion window are real `Instant`-based durations, not a mocked
/// clock), so callers should budget for `DEFAULT_HEARTBEAT_TIMEOUT` +
/// `DEFAULT_SUSPICION_DURATION` (2s + 5s at the time of writing) plus
/// margin.
fn pump_until_peer_failed(nodes: &mut [&mut Runtime], dead: NodeId, timeout: Duration) {
    let deadline = Instant::now() + timeout;
    loop {
        let mut all_failed = true;
        for rt in nodes.iter_mut() {
            rt.process_network();
            let status = rt
                .distributed
                .cluster
                .as_ref()
                .unwrap()
                .get_node(dead)
                .map(|info| info.status);
            if status != Some(NodeStatus::Failed) {
                all_failed = false;
            }
        }
        if all_failed {
            return;
        }
        assert!(
            Instant::now() < deadline,
            "peer {:?} was not marked Failed within the timeout",
            dead
        );
        sleep(Duration::from_millis(100));
    }
}

/// Pump network processing on `nodes` until every node's cluster view of
/// every OTHER node (identified by node id in `listen`) carries that peer's
/// REAL listen address, or the deadline elapses.
///
/// Address convergence is strictly slower than the membership-count
/// convergence `pump_until_converged` checks: the heartbeat discovery path
/// records a joiner's ephemeral SOURCE port (not its listen address), and
/// that stale address propagates through relayed gossip at the baseline
/// incarnation. It is only corrected once the peer's authoritative
/// self-gossip arrives AND the correction is re-propagated at a bumped
/// incarnation (see `merge_membership_from_sender`). Tests that route a
/// remote message to a non-seed node must wait for this, or `send_distributed`
/// dials a dead source port and silently drops the message.
fn pump_until_addresses_converge(
    nodes: &mut [&mut Runtime],
    listen: &[(NodeId, SocketAddr)],
    timeout: Duration,
) {
    let deadline = Instant::now() + timeout;
    loop {
        for rt in nodes.iter_mut() {
            rt.process_network();
        }
        let mut converged = true;
        for rt in nodes.iter() {
            let local = rt.distributed.node_id.unwrap();
            let cluster = rt.distributed.cluster.as_ref().unwrap();
            for &(peer_id, real_addr) in listen {
                if peer_id == local {
                    continue;
                }
                if cluster.get_node(peer_id).map(|i| i.address) != Some(real_addr) {
                    converged = false;
                    break;
                }
            }
            if !converged {
                break;
            }
        }
        if converged {
            return;
        }
        assert!(
            Instant::now() < deadline,
            "cluster addresses did not converge to real listen addresses within the timeout"
        );
        sleep(Duration::from_millis(50));
    }
}

#[cfg(feature = "tcp")]
/// PLAN.md Phase 1 bullet 4 (chaos suite for distribution): a first,
/// real step -- not the full "10^3 seeds across 5 topologies" target,
/// but a genuine fault-injection test against real `Runtime` instances
/// over real loopback TCP (the same infrastructure
/// `test_three_node_cluster_membership_converges` uses), not a
/// simulated stand-in. Covers the core chaos-suite value proposition:
/// a hard node failure (transport killed with no graceful Leave
/// packet, simulating a crash or a network cable pulled, not
/// `leave_cluster()`) is detected by the survivors via heartbeat
/// timeout, the surviving nodes keep operating correctly with each
/// other (both membership-table health AND actual remote message
/// delivery, not just membership bookkeeping), and a fresh node
/// (simulating the crashed node restarting) can rejoin the cluster
/// afterward.
///
/// What this does NOT cover (follow-up, not attempted here): 5-node
/// topologies, split-brain (two healthy sub-clusters that can't see
/// each other, as opposed to one node dying outright), asymmetric
/// partition (A sees B but B can't see A), rolling restart of every
/// node in sequence, and running this shape across many seeds in CI.
#[test]
fn test_three_node_cluster_survives_hard_node_failure_and_rejoin() {
    let mut rt_a = start_distributed_node();
    let mut rt_b = start_distributed_node();
    let mut rt_c = start_distributed_node();

    let addr_a = rt_a.distributed.transport.as_ref().unwrap().listen_addr();
    let addr_b = rt_b.distributed.transport.as_ref().unwrap().listen_addr();
    let node_a = rt_a.distributed.node_id.unwrap();
    let node_b = rt_b.distributed.node_id.unwrap();
    let node_c = rt_c.distributed.node_id.unwrap();

    rt_b.join_cluster(addr_a);
    rt_c.join_cluster(addr_a);
    rt_c.join_cluster(addr_b);
    pump_until_converged(
        &mut [&mut rt_a, &mut rt_b, &mut rt_c],
        3,
        Duration::from_secs(30),
    );

    // An actor on node A that A and B will exchange a remote message
    // through AFTER C dies, proving the survivors keep doing real work
    // together, not just that their membership tables update.
    let actor_id = rt_a.spawn_actor(Box::new(|| vec![("received".to_string(), Value::int(0))]));
    {
        let actor = rt_a.actors.get_mut(&actor_id).unwrap();
        actor.register_behavior("store", |actor, args| {
            let n = args.get(0).and_then(|v| v.as_int()).unwrap_or(-1);
            actor.set_state_field("received", Value::int(n));
        });
    }

    // Kill C's transport hard -- no graceful Leave packet.
    shutdown_nodes(&mut [&mut rt_c]);

    // A and B must detect C's failure via heartbeat timeout + suspicion
    // window (real wall-clock time).
    pump_until_peer_failed(&mut [&mut rt_a, &mut rt_b], node_c, Duration::from_secs(20));

    // A and B remain Healthy to each other throughout -- the cluster
    // survives losing one of three nodes, it doesn't cascade.
    for rt in [&rt_a, &rt_b] {
        let cluster = rt.distributed.cluster.as_ref().unwrap();
        assert_eq!(
            cluster.get_node(node_a).unwrap().status,
            NodeStatus::Healthy
        );
        assert_eq!(
            cluster.get_node(node_b).unwrap().status,
            NodeStatus::Healthy
        );
    }

    // B sends a remote message to A's actor -- real cross-node delivery
    // still works after losing a peer, not just membership bookkeeping.
    let target = ActorAddress::remote(node_a, actor_id);
    rt_b.send_distributed(target, "store", &[Value::int(77)]);
    let deadline = Instant::now() + Duration::from_secs(20);
    let delivered = loop {
        rt_a.process_network();
        rt_b.process_network();
        rt_a.run_scheduler();
        let got = rt_a
            .actors
            .get(&actor_id)
            .and_then(|a| a.get_state_field("received"))
            .and_then(|v| v.as_int());
        if got == Some(77) {
            break true;
        }
        if Instant::now() >= deadline {
            break false;
        }
        sleep(Duration::from_millis(50));
    };
    assert!(
        delivered,
        "remote delivery between surviving nodes failed after losing a peer"
    );

    // Rejoin: a fresh Runtime (simulating node C restarting after its
    // crash, not resuming the old process) joins the surviving cluster.
    let mut rt_c2 = start_distributed_node();
    let node_c2 = rt_c2.distributed.node_id.unwrap();
    rt_c2.join_cluster(addr_a);
    pump_until_converged(
        &mut [&mut rt_a, &mut rt_b, &mut rt_c2],
        3,
        Duration::from_secs(30),
    );
    for rt in [&rt_a, &rt_b, &rt_c2] {
        let cluster = rt.distributed.cluster.as_ref().unwrap();
        assert_eq!(
            cluster.get_node(node_c2).unwrap().status,
            NodeStatus::Healthy,
            "restarted node did not rejoin as healthy"
        );
    }

    shutdown_nodes(&mut [&mut rt_a, &mut rt_b, &mut rt_c2]);
}

#[cfg(feature = "tcp")]
/// PLAN.md Phase 1 bullet 4 (chaos suite) rolling-restart follow-up.
/// `test_three_node_cluster_survives_hard_node_failure_and_rejoin` proved a
/// single hard node failure is detected and the survivors keep operating
/// while a fresh node rejoins; this extends the shape to a FULL rolling
/// restart of every node in the cluster, in sequence. For each node: kill its
/// transport hard (no graceful Leave packet), wait for the remaining live
/// nodes to mark it `Failed` through the real heartbeat-timeout/suspicion
/// machine, then bring up a fresh node (a new process identity, not a resume)
/// that joins the surviving cluster and reconverges to full healthy
/// membership. After the whole cycle the restarted nodes still deliver a
/// remote message to each other — the cluster did real cross-node work
/// throughout every restart, not just membership bookkeeping.
///
/// Deliberately NOT covered here (still follow-up): 5-node topologies,
/// split-brain, asymmetric partition, and multi-seed CI. Split-brain in
/// particular needs a partition-injection primitive over loopback TCP (there
/// is no `tc`/iptables in this test harness), which this test does not
/// attempt.
#[test]
fn test_three_node_cluster_survives_rolling_restart_of_every_node() {
    let mut rt_a = start_distributed_node();
    let mut rt_b = start_distributed_node();
    let mut rt_c = start_distributed_node();

    let addr_a = rt_a.distributed.transport.as_ref().unwrap().listen_addr();
    let addr_b = rt_b.distributed.transport.as_ref().unwrap().listen_addr();
    let node_a = rt_a.distributed.node_id.unwrap();
    let node_b = rt_b.distributed.node_id.unwrap();
    let node_c = rt_c.distributed.node_id.unwrap();

    // Form a full 3-node cluster (chain-seeded through B so gossip converges
    // transitively, mirroring the hard-failure test).
    rt_a.join_cluster(addr_b);
    rt_c.join_cluster(addr_b);
    pump_until_converged(
        &mut [&mut rt_a, &mut rt_b, &mut rt_c],
        3,
        Duration::from_secs(30),
    );

    // Restart C: kill it, let A+B detect the failure, then a fresh C2 joins
    // the surviving cluster through A.
    shutdown_nodes(&mut [&mut rt_c]);
    pump_until_peer_failed(&mut [&mut rt_a, &mut rt_b], node_c, Duration::from_secs(20));
    let mut rt_c2 = start_distributed_node();
    let node_c2 = rt_c2.distributed.node_id.unwrap();
    rt_c2.join_cluster(addr_a);
    pump_until_converged(
        &mut [&mut rt_a, &mut rt_b, &mut rt_c2],
        3,
        Duration::from_secs(30),
    );
    for rt in [&rt_a, &rt_b, &rt_c2] {
        let cluster = rt.distributed.cluster.as_ref().unwrap();
        assert_eq!(
            cluster.get_node(node_c2).unwrap().status,
            NodeStatus::Healthy,
            "C2 did not rejoin healthy after C's restart"
        );
    }

    // Restart B: A+C2 detect the failure, then a fresh B2 joins through C2.
    let addr_c2 = rt_c2.distributed.transport.as_ref().unwrap().listen_addr();
    shutdown_nodes(&mut [&mut rt_b]);
    pump_until_peer_failed(
        &mut [&mut rt_a, &mut rt_c2],
        node_b,
        Duration::from_secs(20),
    );
    let mut rt_b2 = start_distributed_node();
    let node_b2 = rt_b2.distributed.node_id.unwrap();
    rt_b2.join_cluster(addr_c2);
    pump_until_converged(
        &mut [&mut rt_a, &mut rt_c2, &mut rt_b2],
        3,
        Duration::from_secs(30),
    );
    for rt in [&rt_a, &rt_c2, &rt_b2] {
        let cluster = rt.distributed.cluster.as_ref().unwrap();
        assert_eq!(
            cluster.get_node(node_b2).unwrap().status,
            NodeStatus::Healthy,
            "B2 did not rejoin healthy after B's restart"
        );
    }

    // Restart A last (only C2+B2 remain to detect the failure and to join
    // through): a fresh A2 joins through C2.
    shutdown_nodes(&mut [&mut rt_a]);
    pump_until_peer_failed(
        &mut [&mut rt_c2, &mut rt_b2],
        node_a,
        Duration::from_secs(20),
    );
    let mut rt_a2 = start_distributed_node();
    let node_a2 = rt_a2.distributed.node_id.unwrap();
    rt_a2.join_cluster(addr_c2);
    pump_until_converged(
        &mut [&mut rt_c2, &mut rt_b2, &mut rt_a2],
        3,
        Duration::from_secs(30),
    );
    for rt in [&rt_c2, &rt_b2, &rt_a2] {
        let cluster = rt.distributed.cluster.as_ref().unwrap();
        assert_eq!(
            cluster.get_node(node_a2).unwrap().status,
            NodeStatus::Healthy,
            "A2 did not rejoin healthy after A's restart"
        );
    }

    // Every original node has now been restarted once and the cluster is back
    // to full health. Prove the fully-restarted cluster still does real work:
    // B2 sends a remote message to an actor on A2 and it is delivered.
    let actor_id = rt_a2.spawn_actor(Box::new(|| vec![("received".to_string(), Value::int(0))]));
    {
        let actor = rt_a2.actors.get_mut(&actor_id).unwrap();
        actor.register_behavior("store", |actor, args| {
            let n = args.get(0).and_then(|v| v.as_int()).unwrap_or(-1);
            actor.set_state_field("received", Value::int(n));
        });
    }
    let target = ActorAddress::remote(node_a2, actor_id);
    // The three restarted nodes must agree on each other's real listen
    // addresses before a remote send can route (the heartbeat discovery path
    // records ephemeral source ports; the authoritative self-gossip
    // correction must propagate first — see `pump_until_addresses_converge`).
    let addr_a2 = rt_a2.distributed.transport.as_ref().unwrap().listen_addr();
    let addr_b2 = rt_b2.distributed.transport.as_ref().unwrap().listen_addr();
    pump_until_addresses_converge(
        &mut [&mut rt_a2, &mut rt_b2, &mut rt_c2],
        &[(node_a2, addr_a2), (node_b2, addr_b2), (node_c2, addr_c2)],
        Duration::from_secs(15),
    );
    rt_b2.send_distributed(target, "store", &[Value::int(99)]);
    let deadline = Instant::now() + Duration::from_secs(20);
    let delivered = loop {
        rt_a2.process_network();
        rt_b2.process_network();
        rt_a2.run_scheduler();
        let got = rt_a2
            .actors
            .get(&actor_id)
            .and_then(|a| a.get_state_field("received"))
            .and_then(|v| v.as_int());
        if got == Some(99) {
            break true;
        }
        if Instant::now() >= deadline {
            break false;
        }
        sleep(Duration::from_millis(50));
    };
    assert!(
        delivered,
        "remote delivery failed after a full rolling restart of every node"
    );

    shutdown_nodes(&mut [&mut rt_c2, &mut rt_b2, &mut rt_a2]);
}

#[cfg(feature = "tcp")]
/// Start a distributed node with a virtual clock installed AFTER
/// distribution is enabled (the cluster's real-time stamps predate the
/// clock base, so `Instant::duration_since` never underflows). All time
/// queries — heartbeat cadence, failure detection, suspicion, probes —
/// then run on the virtual clock, which the test advances in lockstep
/// across nodes via [`advance_all`].
fn start_virtual_clock_node() -> Runtime {
    let mut rt = start_distributed_node();
    rt.install_virtual_clock();
    rt
}

/// Advance every node's virtual clock by `step`, then pump network
/// processing on each (cluster tick + packet delivery). Advancing first
/// means the tick sees the new virtual time, so heartbeats, suspicion
/// transitions, failure detection, and probes all fire on schedule.
fn advance_all(nodes: &mut [&mut Runtime], step: Duration) {
    for rt in nodes.iter_mut() {
        rt.advance_time(step);
    }
    for rt in nodes.iter_mut() {
        rt.process_network();
    }
    // Let the real loopback TCP threads deliver packets written this
    // round before the next round reads them. 10 ms (was 2 ms): under
    // heavy CI load the reader threads can be delayed past the virtual
    // heartbeat deadline, making heartbeats appear lost and breaking
    // convergence (observed flake: 5-node split-brain test).
    sleep(Duration::from_millis(10));
}

/// The status of `node` in `rt`'s cluster view, if known.
fn cluster_status(rt: &Runtime, node: NodeId) -> Option<NodeStatus> {
    rt.distributed
        .cluster
        .as_ref()
        .and_then(|c| c.get_node(node))
        .map(|info| info.status)
}

/// True once every node has every OTHER node in its ACTIVE view. The
/// failure detector watches only the active view, so this — not
/// `healthy_node_count` — is the real "failure detection is armed"
/// convergence condition. Membership (gossip) converges in ~1 s virtual,
/// but active views fill only through the 5 s repair cycle + reciprocal
/// heartbeat confirmation.
fn active_views_converged(nodes: &[&Runtime], ids: &[NodeId]) -> bool {
    nodes.iter().all(|rt| {
        let c = rt.distributed.cluster.as_ref().unwrap();
        let active: Vec<NodeId> = c.active_view().to_vec();
        let local = rt.distributed.node_id.unwrap();
        ids.iter().all(|id| *id == local || active.contains(id))
    })
}

#[cfg(feature = "tcp")]
/// PLAN.md Phase 1 bullet 4 (chaos suite): split-brain — two mutually
/// invisible healthy sub-clusters, NOT one node dying. Three real
/// `Runtime`s over real loopback TCP, driven by per-node virtual clocks
/// advanced in lockstep so failure detection is deterministic (no real
/// 7 s wall-clock wait). A network partition is injected via
/// [`NetworkTransport::set_partition`] (outbound packets to the other
/// side silently dropped, exactly like a firewall): A and B lose C and
/// vice versa, while A and B keep talking. Asserts both sides detect the
/// other side as `Failed` through the REAL heartbeat-timeout/suspicion
/// machine, that each sub-cluster stays internally `Healthy`, that
/// healing the partition reconverges all three to `Healthy` via the
/// probe/self-healing path (no external rejoin), and that a remote
/// message then delivers across the former partition boundary.
#[test]
fn test_three_node_cluster_split_brain_detects_and_heals() {
    let mut rt_a = start_virtual_clock_node();
    let mut rt_b = start_virtual_clock_node();
    let mut rt_c = start_virtual_clock_node();

    let addr_b = rt_b.distributed.transport.as_ref().unwrap().listen_addr();
    let node_a = rt_a.distributed.node_id.unwrap();
    let node_b = rt_b.distributed.node_id.unwrap();
    let node_c = rt_c.distributed.node_id.unwrap();

    // Form a full 3-node cluster (chain-seeded so gossip converges
    // transitively).
    rt_a.join_cluster(addr_b);
    rt_c.join_cluster(addr_b);
    let step = Duration::from_millis(100);
    let mut converged = false;
    for _ in 0..200 {
        advance_all(&mut [&mut rt_a, &mut rt_b, &mut rt_c], step);
        if active_views_converged(&[&rt_a, &rt_b, &rt_c], &[node_a, node_b, node_c]) {
            converged = true;
            break;
        }
    }
    assert!(
        converged,
        "cluster did not converge to fully-armed failure detection (active views)"
    );

    // Inject the partition: {A,B} | {C}. A and B keep talking to each
    // other; nobody can reach C and C can't reach anyone.
    let ab_partition: HashSet<NodeId> = HashSet::from([node_c]);
    let c_partition: HashSet<NodeId> = HashSet::from([node_a, node_b]);
    rt_a.distributed
        .transport
        .as_mut()
        .unwrap()
        .set_partition(ab_partition.clone());
    rt_b.distributed
        .transport
        .as_mut()
        .unwrap()
        .set_partition(ab_partition.clone());
    rt_c.distributed
        .transport
        .as_mut()
        .unwrap()
        .set_partition(c_partition);

    // Advance virtual time past heartbeat timeout (2 s) + suspicion
    // window (5 s): both sides must mark the other side Failed, while
    // each sub-cluster stays internally Healthy.
    let mut detected = false;
    for _ in 0..200 {
        advance_all(&mut [&mut rt_a, &mut rt_b, &mut rt_c], step);
        let a_sees_c_failed = cluster_status(&rt_a, node_c) == Some(NodeStatus::Failed);
        let b_sees_c_failed = cluster_status(&rt_b, node_c) == Some(NodeStatus::Failed);
        let c_sees_a_failed = cluster_status(&rt_c, node_a) == Some(NodeStatus::Failed);
        let c_sees_b_failed = cluster_status(&rt_c, node_b) == Some(NodeStatus::Failed);
        let a_sees_b_healthy = cluster_status(&rt_a, node_b) == Some(NodeStatus::Healthy);
        let b_sees_a_healthy = cluster_status(&rt_b, node_a) == Some(NodeStatus::Healthy);
        if a_sees_c_failed
            && b_sees_c_failed
            && c_sees_a_failed
            && c_sees_b_failed
            && a_sees_b_healthy
            && b_sees_a_healthy
        {
            detected = true;
            break;
        }
    }
    assert!(
        detected,
        "split-brain not detected: A->C={:?} B->C={:?} C->A={:?} C->B={:?} A->B={:?} B->A={:?}",
        cluster_status(&rt_a, node_c),
        cluster_status(&rt_b, node_c),
        cluster_status(&rt_c, node_a),
        cluster_status(&rt_c, node_b),
        cluster_status(&rt_a, node_b),
        cluster_status(&rt_b, node_a),
    );

    // Heal the partition: clear every node's drop set. The probe path
    // (every 5 s virtual) re-reaches the other side, `handle_heartbeat`
    // re-promotes Failed -> Healthy, and the cluster reconverges with no
    // external rejoin.
    for rt in [&mut rt_a, &mut rt_b, &mut rt_c] {
        rt.distributed
            .transport
            .as_mut()
            .unwrap()
            .set_partition(HashSet::new());
    }
    let mut healed = false;
    for _ in 0..300 {
        advance_all(&mut [&mut rt_a, &mut rt_b, &mut rt_c], step);
        if active_views_converged(&[&rt_a, &rt_b, &rt_c], &[node_a, node_b, node_c]) {
            healed = true;
            break;
        }
    }
    assert!(
        healed,
        "cluster did not heal after the partition was lifted"
    );

    // Prove the healed cluster does real cross-boundary work: C sends a
    // remote message to an actor on A, across the former partition line.
    let actor_id = rt_a.spawn_actor(Box::new(|| vec![("received".to_string(), Value::int(0))]));
    {
        let actor = rt_a.actors.get_mut(&actor_id).unwrap();
        actor.register_behavior("store", |actor, args| {
            let n = args.get(0).and_then(|v| v.as_int()).unwrap_or(-1);
            actor.set_state_field("received", Value::int(n));
        });
    }
    let target = ActorAddress::remote(node_a, actor_id);
    rt_c.send_distributed(target, "store", &[Value::int(33)]);
    let deadline = Instant::now() + Duration::from_secs(20);
    let delivered = loop {
        rt_a.process_network();
        rt_c.process_network();
        rt_a.run_scheduler();
        let got = rt_a
            .actors
            .get(&actor_id)
            .and_then(|a| a.get_state_field("received"))
            .and_then(|v| v.as_int());
        if got == Some(33) {
            break true;
        }
        if Instant::now() >= deadline {
            break false;
        }
        sleep(Duration::from_millis(50));
    };
    assert!(
        delivered,
        "remote delivery across the healed split-brain boundary failed"
    );

    shutdown_nodes(&mut [&mut rt_a, &mut rt_b, &mut rt_c]);
}

#[cfg(feature = "tcp")]
/// PLAN.md Phase 1 bullet 4 (chaos suite): asymmetric partition — A sees
/// B but B can't see A. Only A's outbound packets to B are dropped, so B
/// stops hearing A and must mark A `Failed` through the real failure
/// detector, while A keeps receiving B's heartbeats and keeps B
/// `Healthy`. Asserts the one-directional visibility, then heals and
/// confirms a remote message flows both ways afterward.
#[test]
fn test_three_node_cluster_asymmetric_partition_detects_and_heals() {
    let mut rt_a = start_virtual_clock_node();
    let mut rt_b = start_virtual_clock_node();

    let addr_a = rt_a.distributed.transport.as_ref().unwrap().listen_addr();
    let node_a = rt_a.distributed.node_id.unwrap();
    let node_b = rt_b.distributed.node_id.unwrap();

    rt_b.join_cluster(addr_a);
    let step = Duration::from_millis(100);
    let mut converged = false;
    for _ in 0..200 {
        advance_all(&mut [&mut rt_a, &mut rt_b], step);
        if active_views_converged(&[&rt_a, &rt_b], &[node_a, node_b]) {
            converged = true;
            break;
        }
    }
    assert!(
        converged,
        "cluster did not converge to fully-armed failure detection (active views)"
    );

    // Asymmetric partition: A cannot reach B, but B can reach A.
    rt_a.distributed
        .transport
        .as_mut()
        .unwrap()
        .set_partition(HashSet::from([node_b]));

    // B must mark A Failed (it hears nothing from A) while A keeps B
    // Healthy (B's heartbeats still arrive).
    let mut detected = false;
    for _ in 0..200 {
        advance_all(&mut [&mut rt_a, &mut rt_b], step);
        if cluster_status(&rt_b, node_a) == Some(NodeStatus::Failed)
            && cluster_status(&rt_a, node_b) == Some(NodeStatus::Healthy)
        {
            detected = true;
            break;
        }
    }
    assert!(
        detected,
        "asymmetric partition not detected: B->A={:?} A->B={:?}",
        cluster_status(&rt_b, node_a),
        cluster_status(&rt_a, node_b),
    );

    // Heal and reconverge.
    rt_a.distributed
        .transport
        .as_mut()
        .unwrap()
        .set_partition(HashSet::new());
    let mut healed = false;
    for _ in 0..300 {
        advance_all(&mut [&mut rt_a, &mut rt_b], step);
        if active_views_converged(&[&rt_a, &rt_b], &[node_a, node_b]) {
            healed = true;
            break;
        }
    }
    assert!(
        healed,
        "cluster did not heal after the asymmetric partition"
    );

    // Both directions deliver after healing.
    for (want, spawn_on_a) in [(11, true), (22, false)] {
        let target_node = if spawn_on_a { node_a } else { node_b };
        let actor_id = if spawn_on_a {
            rt_a.spawn_actor(Box::new(|| vec![("received".to_string(), Value::int(0))]))
        } else {
            rt_b.spawn_actor(Box::new(|| vec![("received".to_string(), Value::int(0))]))
        };
        {
            let actor = if spawn_on_a {
                rt_a.actors.get_mut(&actor_id).unwrap()
            } else {
                rt_b.actors.get_mut(&actor_id).unwrap()
            };
            actor.register_behavior("store", |actor, args| {
                let n = args.get(0).and_then(|v| v.as_int()).unwrap_or(-1);
                actor.set_state_field("received", Value::int(n));
            });
        }
        let target = ActorAddress::remote(target_node, actor_id);
        if spawn_on_a {
            rt_b.send_distributed(target, "store", &[Value::int(want)]);
        } else {
            rt_a.send_distributed(target, "store", &[Value::int(want)]);
        }
        let deadline = Instant::now() + Duration::from_secs(20);
        let delivered = loop {
            rt_a.process_network();
            rt_b.process_network();
            if spawn_on_a {
                rt_a.run_scheduler();
            } else {
                rt_b.run_scheduler();
            }
            let got = if spawn_on_a {
                rt_a.actors
                    .get(&actor_id)
                    .and_then(|a| a.get_state_field("received"))
                    .and_then(|v| v.as_int())
            } else {
                rt_b.actors
                    .get(&actor_id)
                    .and_then(|a| a.get_state_field("received"))
                    .and_then(|v| v.as_int())
            };
            if got == Some(want) {
                break true;
            }
            if Instant::now() >= deadline {
                break false;
            }
            sleep(Duration::from_millis(50));
        };
        assert!(delivered, "remote delivery failed after heal (want {want})");
    }

    shutdown_nodes(&mut [&mut rt_a, &mut rt_b]);
}

#[cfg(feature = "tcp")]
/// PLAN.md Phase 1 bullet 4 (chaos suite): 5-node split-brain, the
/// "5-node topologies" item. Five real `Runtime`s over real loopback
/// TCP, split {A,B} | {C,D,E} via transport-level packet drops. Asserts
/// every cross-side pair is marked `Failed` on both sides, every
/// intra-side pair stays `Healthy` throughout, healing reconverges all
/// five, and a remote message delivers across the healed boundary.
#[test]
fn test_five_node_cluster_split_brain_detects_and_heals() {
    let mut nodes: Vec<Runtime> = (0..5).map(|_| start_virtual_clock_node()).collect();
    let addrs: Vec<SocketAddr> = nodes
        .iter()
        .map(|rt| rt.distributed.transport.as_ref().unwrap().listen_addr())
        .collect();
    let ids: Vec<NodeId> = nodes
        .iter()
        .map(|rt| rt.distributed.node_id.unwrap())
        .collect();

    // Chain-seed: everyone joins through node 0, so gossip converges
    // transitively.
    for i in 1..5 {
        nodes[i].join_cluster(addrs[0]);
    }
    let step = Duration::from_millis(100);
    let mut converged = false;
    // 600 iterations (60 s virtual): convergence normally takes ~10 s
    // virtual (5 s repair cycle + reciprocal heartbeat confirmation),
    // but under heavy CI load the real TCP delivery can lag the virtual
    // clock, so give the budget 2x headroom (observed flake).
    for _ in 0..600 {
        advance_all(&mut nodes.iter_mut().collect::<Vec<_>>(), step);
        if active_views_converged(&nodes.iter().collect::<Vec<_>>(), &ids) {
            converged = true;
            break;
        }
    }
    assert!(
        converged,
        "5-node cluster did not converge to fully-armed failure detection (active views)"
    );

    // Split {A,B} | {C,D,E}.
    let ab_side: HashSet<NodeId> = HashSet::from([ids[2], ids[3], ids[4]]);
    let cde_side: HashSet<NodeId> = HashSet::from([ids[0], ids[1]]);
    for i in 0..2 {
        nodes[i]
            .distributed
            .transport
            .as_mut()
            .unwrap()
            .set_partition(ab_side.clone());
    }
    for i in 2..5 {
        nodes[i]
            .distributed
            .transport
            .as_mut()
            .unwrap()
            .set_partition(cde_side.clone());
    }

    // Every cross-side pair fails on both sides; every intra-side pair
    // stays healthy.
    let mut detected = false;
    for _ in 0..300 {
        advance_all(&mut nodes.iter_mut().collect::<Vec<_>>(), step);
        let cross_failed = (0..2)
            .flat_map(|i| (2..5).map(move |j| (i, j)))
            .all(|(i, j)| {
                cluster_status(&nodes[i], ids[j]) == Some(NodeStatus::Failed)
                    && cluster_status(&nodes[j], ids[i]) == Some(NodeStatus::Failed)
            });
        let intra_healthy = (0..2)
            .flat_map(|i| (0..2).map(move |j| (i, j)))
            .chain((2..5).flat_map(|i| (2..5).map(move |j| (i, j))))
            .filter(|(i, j)| i != j)
            .all(|(i, j)| cluster_status(&nodes[i], ids[j]) == Some(NodeStatus::Healthy));
        if cross_failed && intra_healthy {
            detected = true;
            break;
        }
    }
    assert!(
        detected,
        "5-node split-brain not detected: A->C={:?} C->A={:?} A->B={:?} C->D={:?}",
        cluster_status(&nodes[0], ids[2]),
        cluster_status(&nodes[2], ids[0]),
        cluster_status(&nodes[0], ids[1]),
        cluster_status(&nodes[2], ids[3]),
    );

    // Heal and reconverge all five.
    for node in nodes.iter_mut() {
        node.distributed
            .transport
            .as_mut()
            .unwrap()
            .set_partition(HashSet::new());
    }
    let mut healed = false;
    for _ in 0..400 {
        advance_all(&mut nodes.iter_mut().collect::<Vec<_>>(), step);
        if active_views_converged(&nodes.iter().collect::<Vec<_>>(), &ids) {
            healed = true;
            break;
        }
    }
    assert!(healed, "5-node cluster did not heal after the split-brain");

    // Cross-boundary delivery after healing: E (node 4) -> actor on A
    // (node 0).
    let actor_id = nodes[0].spawn_actor(Box::new(|| vec![("received".to_string(), Value::int(0))]));
    {
        let actor = nodes[0].actors.get_mut(&actor_id).unwrap();
        actor.register_behavior("store", |actor, args| {
            let n = args.get(0).and_then(|v| v.as_int()).unwrap_or(-1);
            actor.set_state_field("received", Value::int(n));
        });
    }
    let target = ActorAddress::remote(ids[0], actor_id);
    nodes[4].send_distributed(target, "store", &[Value::int(44)]);
    let deadline = Instant::now() + Duration::from_secs(20);
    let delivered = loop {
        nodes[0].process_network();
        nodes[4].process_network();
        nodes[0].run_scheduler();
        let got = nodes[0]
            .actors
            .get(&actor_id)
            .and_then(|a| a.get_state_field("received"))
            .and_then(|v| v.as_int());
        if got == Some(44) {
            break true;
        }
        if Instant::now() >= deadline {
            break false;
        }
        sleep(Duration::from_millis(50));
    };
    assert!(
        delivered,
        "remote delivery across the healed 5-node split-brain boundary failed"
    );

    shutdown_nodes(&mut nodes.iter_mut().collect::<Vec<_>>());
}

#[cfg(feature = "tcp")]
/// PLAN.md Phase 1 bullet 4 (chaos suite): split-brain RESOLVER behavior
/// end-to-end — the `StaticQuorumResolver` down-self path through the
/// REAL runtime, not the cluster-sim unit tests. Three real `Runtime`s
/// over real loopback TCP, configured with `StaticQuorum{3}` (quorum =
/// 3/2 + 1 = 2), driven by per-node virtual clocks advanced in lockstep
/// so the failure detection + resolver decision happen in virtual time.
/// A clean partition isolates A from {B,C}: A sees only itself (1 < 2
/// reachable) so the resolver downs A — the transport shuts down and
/// local actors keep running — while B and C see each other (2 >= 2)
/// and stay up. Healing the partition does NOT resurrect A (its
/// transport is gone; operator restart is the recovery path), and the
/// surviving majority keeps working.
#[test]
fn test_three_node_cluster_static_quorum_downs_minority() {
    let quorum_config = ClusterConfig {
        split_brain: SplitBrainConfig::StaticQuorum { expected_nodes: 3 },
        probe_interval: Duration::from_secs(5),
        ..Default::default()
    };

    // Set the config BEFORE enable_distribution so `apply_config` picks
    // up the resolver at enable time.
    let mut rt_a = start_distributed_node();
    rt_a.set_cluster_config(quorum_config.clone());
    rt_a.install_virtual_clock();
    let mut rt_b = start_distributed_node();
    rt_b.set_cluster_config(quorum_config.clone());
    rt_b.install_virtual_clock();
    let mut rt_c = start_distributed_node();
    rt_c.set_cluster_config(quorum_config.clone());
    rt_c.install_virtual_clock();

    let addr_b = rt_b.distributed.transport.as_ref().unwrap().listen_addr();
    let node_a = rt_a.distributed.node_id.unwrap();
    let node_b = rt_b.distributed.node_id.unwrap();
    let node_c = rt_c.distributed.node_id.unwrap();

    rt_a.join_cluster(addr_b);
    rt_c.join_cluster(addr_b);

    // A local actor on A before the partition, so we can prove it keeps
    // running after A is downed.
    let actor_id = rt_a.spawn_actor(Box::new(|| vec![("received".to_string(), Value::int(0))]));
    {
        let actor = rt_a.actors.get_mut(&actor_id).unwrap();
        actor.register_behavior("store", |actor, args| {
            let n = args.get(0).and_then(|v| v.as_int()).unwrap_or(-1);
            actor.set_state_field("received", Value::int(n));
        });
    }

    let step = Duration::from_millis(100);
    let mut converged = false;
    for _ in 0..200 {
        advance_all(&mut [&mut rt_a, &mut rt_b, &mut rt_c], step);
        if active_views_converged(&[&rt_a, &rt_b, &rt_c], &[node_a, node_b, node_c]) {
            converged = true;
            break;
        }
    }
    assert!(
        converged,
        "cluster did not converge to fully-armed failure detection (active views)"
    );

    // Clean partition: isolate A from {B,C} in both directions. B and C
    // keep talking.
    let a_side: HashSet<NodeId> = HashSet::from([node_b, node_c]);
    let bc_side: HashSet<NodeId> = HashSet::from([node_a]);
    rt_a.distributed
        .transport
        .as_mut()
        .unwrap()
        .set_partition(a_side.clone());
    rt_b.distributed
        .transport
        .as_mut()
        .unwrap()
        .set_partition(bc_side.clone());
    rt_c.distributed
        .transport
        .as_mut()
        .unwrap()
        .set_partition(bc_side.clone());

    // A must down itself (1 < 2 reachable) once its detector marks B and
    // C Failed; B and C must stay up (2 >= 2 reachable).
    let mut downed = false;
    for _ in 0..200 {
        advance_all(&mut [&mut rt_a, &mut rt_b, &mut rt_c], step);
        let a_down = rt_a.distributed.cluster.as_ref().unwrap().is_down();
        let b_up = !rt_b.distributed.cluster.as_ref().unwrap().is_down();
        let c_up = !rt_c.distributed.cluster.as_ref().unwrap().is_down();
        if a_down && b_up && c_up {
            downed = true;
            break;
        }
    }
    assert!(
        downed,
        "static quorum 3 did not down the isolated minority: A_down={} B_up={} C_up={}",
        rt_a.distributed.cluster.as_ref().unwrap().is_down(),
        !rt_b.distributed.cluster.as_ref().unwrap().is_down(),
        !rt_c.distributed.cluster.as_ref().unwrap().is_down(),
    );

    // The Down action shut A's transport down (threads joined, streams
    // closed) — A no longer participates. The durable observable is the
    // cluster's `local_down` flag, already asserted; A's frozen view must
    // no longer count the majority as reachable (it stopped ticking once
    // down, so B/C are Suspicious at worst — they stopped being counted
    // as reachable the moment the detector flagged them).
    let a_view_b = cluster_status(&rt_a, node_b).unwrap_or(NodeStatus::Failed);
    assert_ne!(
        a_view_b,
        NodeStatus::Healthy,
        "downed node's view must no longer count the majority as reachable"
    );
    // B and C still see each other healthy.
    assert_eq!(
        cluster_status(&rt_b, node_c),
        Some(NodeStatus::Healthy),
        "majority members must stay healthy to each other"
    );
    assert_eq!(
        cluster_status(&rt_c, node_b),
        Some(NodeStatus::Healthy),
        "majority members must stay healthy to each other"
    );

    // Local actors on the downed node keep running (the Down handler
    // stops network participation only).
    rt_a.send_message(actor_id, "store", &[Value::int(77)]);
    rt_a.run_scheduler();
    let got = rt_a
        .actors
        .get(&actor_id)
        .and_then(|a| a.get_state_field("received"))
        .and_then(|v| v.as_int());
    assert_eq!(
        got,
        Some(77),
        "local actors must keep running on a downed node"
    );

    // Heal the partition: A stays down (transport shut down; operator
    // restart is the recovery path) and the majority keeps working.
    rt_a.distributed
        .transport
        .as_mut()
        .unwrap()
        .set_partition(HashSet::new());
    rt_b.distributed
        .transport
        .as_mut()
        .unwrap()
        .set_partition(HashSet::new());
    rt_c.distributed
        .transport
        .as_mut()
        .unwrap()
        .set_partition(HashSet::new());
    for _ in 0..100 {
        advance_all(&mut [&mut rt_a, &mut rt_b, &mut rt_c], step);
    }
    assert!(
        rt_a.distributed.cluster.as_ref().unwrap().is_down(),
        "downed node must stay down after the partition heals"
    );
    assert!(
        !rt_b.distributed.cluster.as_ref().unwrap().is_down(),
        "majority node B must stay up after heal"
    );
    assert!(
        !rt_c.distributed.cluster.as_ref().unwrap().is_down(),
        "majority node C must stay up after heal"
    );

    // The majority keeps doing real work: B sends a remote message to an
    // actor on C after the heal.
    let actor_c = rt_c.spawn_actor(Box::new(|| vec![("received".to_string(), Value::int(0))]));
    {
        let actor = rt_c.actors.get_mut(&actor_c).unwrap();
        actor.register_behavior("store", |actor, args| {
            let n = args.get(0).and_then(|v| v.as_int()).unwrap_or(-1);
            actor.set_state_field("received", Value::int(n));
        });
    }
    let target = ActorAddress::remote(node_c, actor_c);
    rt_b.send_distributed(target, "store", &[Value::int(88)]);
    let deadline = Instant::now() + Duration::from_secs(20);
    let delivered = loop {
        rt_b.process_network();
        rt_c.process_network();
        rt_c.run_scheduler();
        let got = rt_c
            .actors
            .get(&actor_c)
            .and_then(|a| a.get_state_field("received"))
            .and_then(|v| v.as_int());
        if got == Some(88) {
            break true;
        }
        if Instant::now() >= deadline {
            break false;
        }
        sleep(Duration::from_millis(50));
    };
    assert!(
        delivered,
        "majority nodes must keep delivering remote messages after the minority was downed"
    );

    shutdown_nodes(&mut [&mut rt_a, &mut rt_b, &mut rt_c]);
}

#[cfg(feature = "tcp")]
/// End-to-end coverage of PLAN.md Phase 5 deliverable 7 parts (a)+(b)
/// through the REAL failure-detection path, not a direct call to
/// `handle_node_failed`. A local actor on survivor A monitors a remote
/// actor on node B; B's transport is killed hard (no graceful Leave);
/// A's failure detector marks B `Failed`; that `NodeFailed` action runs
/// `handle_node_failed`, which (a) invalidates A's `RemoteActorCache`
/// entry for B's actor and (b) delivers `DOWN`-with-`noconnection`
/// (payload code 6) to the local watcher. The cross-node registration
/// is set up on the registry directly because the D8 *send* side
/// (`perform Actor.monitor` reaching a remote target) is not yet wired —
/// the receive side (`Packet::Monitor` → `remote_monitors.register`) is
/// exercised by the packet round-trip test; this test drives the
/// failure→DOWN half of the chain.
#[test]
fn test_node_death_delivers_down_to_local_watcher_via_failure_detector() {
    use crate::runtime::supervision::RemoteLink;

    let mut rt_a = start_distributed_node();
    let mut rt_b = start_distributed_node();
    let addr_a = rt_a.distributed.transport.as_ref().unwrap().listen_addr();
    let node_b = rt_b.distributed.node_id.unwrap();
    let local_node_a = rt_a.distributed.node_id.unwrap();

    rt_b.join_cluster(addr_a);
    pump_until_converged(&mut [&mut rt_a, &mut rt_b], 2, Duration::from_secs(30));

    // The remote actor B hosts, and a local watcher on A.
    let remote_target_id = 5555u64;
    let watcher = rt_a.spawn_actor(Box::new(|| vec![]));

    // Register A's local watcher as monitoring a remote actor on B.
    // (See test doc: D8 send-side wiring is not yet present, so register
    // directly; the DOWN delivery path is what we're proving.)
    let target = RemoteLink {
        node_id: node_b,
        actor_id: remote_target_id,
    };
    rt_a.remote_monitors.register(
        target,
        RemoteLink {
            node_id: local_node_a,
            actor_id: watcher,
        },
    );

    // Prime A's remote cache with the doomed node's actor so we can prove
    // invalidation fires too.
    rt_a.distributed
        .resolver
        .as_mut()
        .unwrap()
        .record_remote_send(node_b, remote_target_id);
    assert!(
        rt_a.distributed
            .resolver
            .as_mut()
            .unwrap()
            .cache_mut()
            .get(node_b, remote_target_id)
            .is_some(),
        "cache must hold the doomed remote actor before failure"
    );

    // Kill B's transport hard — no graceful Leave packet.
    shutdown_nodes(&mut [&mut rt_b]);

    // A must detect B's failure via heartbeat timeout + suspicion window
    // (real wall-clock time), which fires `handle_node_failed`.
    pump_until_peer_failed(&mut [&mut rt_a], node_b, Duration::from_secs(20));

    // (a) A's cache entry for B's actor is invalidated.
    assert!(
        rt_a.distributed
            .resolver
            .as_mut()
            .unwrap()
            .cache_mut()
            .get(node_b, remote_target_id)
            .is_none(),
        "cache entry for the failed node's actor must be invalidated"
    );

    // (b) The local watcher received DOWN-with-noconnection (code 6).
    let down = rt_a
        .actors
        .get_mut(&watcher)
        .unwrap()
        .mailbox
        .pop()
        .expect("local watcher must receive a DOWN when the remote node dies");
    assert_eq!(down.behavior_id, 0, "DOWN is a system message");
    assert_eq!(
        down.payload[0].as_int(),
        Some(remote_target_id as i64),
        "DOWN target must be the remote actor id"
    );
    assert_eq!(down.payload[1].as_int(), Some(watcher as i64));
    assert_eq!(down.payload[2].as_int(), Some(6), "noconnection code");

    // The registry entry for the dead node is dropped.
    assert!(
        rt_a.remote_monitors.get_watchers(target).is_none(),
        "monitor registry entry for the dead node must be dropped"
    );

    shutdown_nodes(&mut [&mut rt_a]);
}

#[cfg(feature = "tcp")]
/// The self-healing path (Phase 5 deliverable 2): when a node goes quiet,
/// the survivor's failure detector marks it Failed and the cluster probe
/// re-establishes contact — once the quiet node processes again, the probe
/// heartbeat re-promotes it with no explicit `join_cluster` on its side.
#[test]
fn test_probe_rejoins_quiet_node_without_explicit_join() {
    let mut rt_a = start_distributed_node();
    let mut rt_b = start_distributed_node();
    let addr_a = rt_a.distributed.transport.as_ref().unwrap().listen_addr();
    let node_b = rt_b.distributed.node_id.unwrap();

    rt_b.join_cluster(addr_a);
    pump_until_converged(&mut [&mut rt_a, &mut rt_b], 2, Duration::from_secs(30));

    // Simulate B going quiet: stop pumping it. Its transport threads stay
    // alive (it can still receive), but its cluster tick never runs, so it
    // sends no heartbeats. Pump only A until its failure detector marks B
    // Failed (real wall-clock suspicion window).
    let deadline = Instant::now() + Duration::from_secs(30);
    loop {
        rt_a.process_network();
        let status = rt_a
            .distributed
            .cluster
            .as_ref()
            .unwrap()
            .get_node(node_b)
            .map(|n| n.status);

        if status == Some(NodeStatus::Failed) {
            break;
        }
        assert!(
            Instant::now() < deadline,
            "A did not mark the quiet node B failed (status: {:?})",
            status
        );
        sleep(Duration::from_millis(50));
    }

    // Resume pumping B. A's probe (a Heartbeat packet to B's address) is
    // already queued on B; B processes it, heartbeats back, and A promotes
    // B to Healthy. B never calls join_cluster again.
    pump_until_converged(&mut [&mut rt_a, &mut rt_b], 2, Duration::from_secs(45));
    assert_eq!(
        rt_a.distributed
            .cluster
            .as_ref()
            .unwrap()
            .get_node(node_b)
            .unwrap()
            .status,
        NodeStatus::Healthy,
        "the probed node must be re-promoted to Healthy without an explicit join"
    );

    shutdown_nodes(&mut [&mut rt_a, &mut rt_b]);
}

#[cfg(feature = "tcp")]
/// Content hash mismatch triggers bytecode fetch; the retry queue holds
/// the message until the FetchBehaviorResponse arrives, then delivers it.
#[test]
fn test_message_retry_after_bytecode_fetch() {
    use crate::bytecode::{BehaviorTableEntry, CodeModule};
    use crate::runtime::network::Packet;

    let mut rt_a = start_distributed_node();
    let mut rt_b = start_distributed_node();

    let addr_a = rt_a.distributed.transport.as_ref().unwrap().listen_addr();
    let node_a = rt_a.distributed.node_id.unwrap();
    let node_b = rt_b.distributed.node_id.unwrap();
    rt_b.join_cluster(addr_a);
    pump_until_converged(&mut [&mut rt_a, &mut rt_b], 2, Duration::from_secs(30));

    // Build a module with a BehaviorTableEntry that has a content_hash.
    // The entry sits at index 0, matching the native handler registered below.
    let module = {
        let mut m = CodeModule::new("test_fetch");
        m.behaviors.push(BehaviorTableEntry {
            name: "store".to_string(),
            param_count: 1,
            code_offset: 0,
            local_count: 0,
            effect_mask: 0,
            compensate_offset: None,
            content_hash: Some([0xAB; 32]),
            source_location: None,
            parallel_branches: None,
        });
        m
    };

    // Node B caches the "correct" module (different hash = [0xCD; 32]).
    // When A fetches this, the retried message's behavior lookup will succeed
    // because hot_reload_behavior replaces A's module with this one.
    let correct_module = {
        let mut m = module.clone();
        m.behaviors[0].content_hash = Some([0xCD; 32]);
        // The hash we use for cache key must match what A will look up.
        m
    };
    let correct_hash: [u8; 32] = [0xCD; 32];
    rt_b.behavior_cache
        .insert(correct_hash, correct_module.clone());

    // Spawn an actor on A with the module (hash [0xAB; 32]) and a native handler.
    let actor_id = rt_a.spawn_actor(Box::new(|| vec![("received".to_string(), Value::int(0))]));
    {
        let actor = rt_a.actors.get_mut(&actor_id).unwrap();
        actor.bytecode_module = Some(module);
        actor.bytecode_offsets = vec![0];
        actor.register_behavior("store", |actor, args| {
            let n = args.get(0).and_then(|v| v.as_int()).unwrap_or(-1);
            actor.set_state_field("received", Value::int(n));
        });
    }

    // Send a message from B to A with correct_hash. A's verify_behavior_hash
    // compares correct_hash against the module's entry (which has [0xAB; 32]).
    // They differ, so A checks its behavior_cache (miss), then sends a
    // FetchBehaviorRequest to B. B has the module cached under correct_hash.
    let packet = Packet::ActorMessage {
        target_actor: actor_id,
        behavior_name: "store".to_string(),
        content_hash: Some(correct_hash),
        payload: vec![Value::int(42)],
        string_table: vec![],
        object_table: vec![],
        sender_actor: 0,
        sender_node: node_b,
        priority: crate::runtime::mailbox::MessagePriority::Normal,
        trace_id: None,
    };
    rt_b.distributed
        .transport
        .as_mut()
        .unwrap()
        .send(node_a, addr_a, packet);

    // Pump both nodes until the message is delivered via retry.
    let deadline = Instant::now() + Duration::from_secs(30);
    let delivered = loop {
        rt_a.process_network();
        rt_b.process_network();
        rt_a.run_scheduler();
        let got = rt_a
            .actors
            .get(&actor_id)
            .and_then(|a| a.get_state_field("received"))
            .and_then(|v| v.as_int());
        if got == Some(42) {
            break true;
        }
        if Instant::now() >= deadline {
            break false;
        }
        std::thread::sleep(Duration::from_millis(50));
    };
    assert!(
        delivered,
        "message should be retried and delivered after bytecode fetch"
    );
    assert!(
        rt_a.pending_fetched_messages.is_empty(),
        "pending_fetched_messages should be drained after retry"
    );

    shutdown_nodes(&mut [&mut rt_a, &mut rt_b]);
}

#[cfg(feature = "tcp")]
/// Gossip relay convergence: three nodes seeded only as a chain
/// (B joins A, C joins B — C never contacts A directly) must still
/// converge to a full membership view via gossip relayed by B.
#[test]
fn test_three_node_gossip_converges_chain_seeded() {
    let mut rt_a = start_distributed_node();
    let mut rt_b = start_distributed_node();
    let mut rt_c = start_distributed_node();

    let addr_a = rt_a.distributed.transport.as_ref().unwrap().listen_addr();
    let addr_b = rt_b.distributed.transport.as_ref().unwrap().listen_addr();
    let node_a = rt_a.distributed.node_id.unwrap();
    let node_c = rt_c.distributed.node_id.unwrap();

    // Chain seeding only: B joins A, C joins B. Without gossip on the
    // wire, A and C could never learn about each other.
    rt_b.join_cluster(addr_a);
    rt_c.join_cluster(addr_b);

    pump_until_converged(
        &mut [&mut rt_a, &mut rt_b, &mut rt_c],
        3,
        Duration::from_secs(30),
    );

    // A learned about C (and vice versa) purely through B's gossip relay,
    // and both views consider the relayed peer healthy.
    let info_c_on_a = rt_a
        .distributed
        .cluster
        .as_ref()
        .unwrap()
        .get_node(node_c)
        .expect("node C missing from A's membership table — gossip relay failed");
    assert_eq!(
        info_c_on_a.status,
        NodeStatus::Healthy,
        "A should see C as healthy"
    );
    let info_a_on_c = rt_c
        .distributed
        .cluster
        .as_ref()
        .unwrap()
        .get_node(node_a)
        .expect("node A missing from C's membership table — gossip relay failed");
    assert_eq!(
        info_a_on_c.status,
        NodeStatus::Healthy,
        "C should see A as healthy"
    );
    // Sanity: the middle node sees both ends.
    let cluster_b = rt_b.distributed.cluster.as_ref().unwrap();
    assert!(cluster_b.is_member(node_a));
    assert!(cluster_b.is_member(node_c));

    shutdown_nodes(&mut [&mut rt_a, &mut rt_b, &mut rt_c]);
}

/// Handler for the remotely-spawnable behavior used by
/// `test_remote_spawn_request_delivery`.
fn remote_spawn_store_handler(actor: &mut Actor, args: &[Value]) {
    let n = args.get(0).and_then(|v| v.as_int()).unwrap_or(-1);
    actor.set_state_field("received", Value::int(n));
}

#[cfg(feature = "tcp")]
/// Remote spawn delivery: node A issues a SpawnRequest for a behavior
/// registered on node B, receives the new actor's id via SpawnResponse,
/// and can then address the spawned actor by name.
#[test]
fn test_remote_spawn_request_delivery() {
    let mut rt_a = start_distributed_node();
    let mut rt_b = start_distributed_node();

    let addr_b = rt_b.distributed.transport.as_ref().unwrap().listen_addr();
    let node_b = rt_b.distributed.node_id.unwrap();

    // Node B offers one behavior for remote spawn.
    rt_b.register_spawnable_behavior("store", remote_spawn_store_handler);

    rt_a.join_cluster(addr_b);
    pump_until_converged(&mut [&mut rt_a, &mut rt_b], 2, Duration::from_secs(30));

    // Issue the remote spawn. The placeholder address carries the request
    // id; the real actor id arrives with the SpawnResponse.
    let request_id = {
        let mut transport = rt_a.distributed.transport.take().unwrap();
        let cluster = rt_a.distributed.cluster.take().unwrap();
        let resolver = rt_a.distributed.resolver.take().unwrap();
        let placeholder = spawn_on_node(
            &mut rt_a,
            &mut transport,
            &cluster,
            &resolver,
            node_b,
            "store",
            vec![("received".to_string(), Value::int(0))],
        );
        rt_a.distributed.transport = Some(transport);
        rt_a.distributed.cluster = Some(cluster);
        rt_a.distributed.resolver = Some(resolver);
        assert_eq!(placeholder.node_id(), node_b);
        placeholder.actor_id()
    };

    let deadline = Instant::now() + Duration::from_secs(30);
    let remote_actor = loop {
        rt_a.process_network();
        rt_b.process_network();
        if let Some(result) = rt_a.take_spawn_response(request_id) {
            break result.expect("node B rejected the spawn request");
        }
        assert!(
            Instant::now() < deadline,
            "no SpawnResponse received from node B"
        );
        sleep(Duration::from_millis(50));
    };

    // The spawned actor exists on node B and was wired with the behavior.
    assert!(
        rt_b.actors.contains_key(&remote_actor),
        "spawned actor missing on node B"
    );
    assert_eq!(
        rt_b.behavior_id_for(remote_actor, "store"),
        Some(0),
        "spawned actor should have the requested behavior at index 0"
    );

    // Node A can now address the remote actor by id; a message sent by
    // behavior name must land in the spawned actor's state.
    let target = ActorAddress::remote(node_b, remote_actor);
    rt_a.send_distributed(target, "store", &[Value::int(7)]);

    let deadline = Instant::now() + Duration::from_secs(30);
    let delivered = loop {
        rt_a.process_network();
        rt_b.process_network();
        rt_b.run_scheduler();
        let got = rt_b
            .actors
            .get(&remote_actor)
            .and_then(|a| a.get_state_field("received"))
            .and_then(|v| v.as_int());
        if got == Some(7) {
            break true;
        }
        if Instant::now() >= deadline {
            break false;
        }
        sleep(Duration::from_millis(50));
    };
    assert!(
        delivered,
        "message to the remotely-spawned actor was not delivered"
    );

    // Unknown behavior names are rejected, not spawned — the no-crash
    // counterpart of the local unknown-behavior fallback.
    let reject_id = {
        let mut transport = rt_a.distributed.transport.take().unwrap();
        let cluster = rt_a.distributed.cluster.take().unwrap();
        let resolver = rt_a.distributed.resolver.take().unwrap();
        let placeholder = spawn_on_node(
            &mut rt_a,
            &mut transport,
            &cluster,
            &resolver,
            node_b,
            "no_such_behavior",
            vec![],
        );
        rt_a.distributed.transport = Some(transport);
        rt_a.distributed.cluster = Some(cluster);
        rt_a.distributed.resolver = Some(resolver);
        placeholder.actor_id()
    };
    let actors_before = rt_b.actors.len();
    let deadline = Instant::now() + Duration::from_secs(30);
    let rejected = loop {
        rt_a.process_network();
        rt_b.process_network();
        if let Some(result) = rt_a.take_spawn_response(reject_id) {
            break result.is_none();
        }
        assert!(
            Instant::now() < deadline,
            "no SpawnResponse received for the unknown behavior"
        );
        sleep(Duration::from_millis(50));
    };
    assert!(rejected, "unknown behavior name must be rejected");
    assert_eq!(
        rt_b.actors.len(),
        actors_before,
        "rejected spawn must not create an actor"
    );

    shutdown_nodes(&mut [&mut rt_a, &mut rt_b]);
}

#[cfg(feature = "tcp")]
/// RFC-0007 cross-node routing by BARE actor-ref value: after a remote
/// spawn, `send`/`ask` addressing the spawned actor by its plain id (the
/// only thing an actor-ref Value carries) must route over the wire —
/// previously the node id was dropped at `remote_spawn` and the bare id
/// fell into the local mailbox path.
#[test]
fn test_remote_ref_send_by_bare_id_routes_wire() {
    let mut rt_a = start_distributed_node();
    let mut rt_b = start_distributed_node();

    let addr_b = rt_b.distributed.transport.as_ref().unwrap().listen_addr();
    let node_b = rt_b.distributed.node_id.unwrap();

    rt_b.register_spawnable_behavior("store", remote_spawn_store_handler);

    rt_a.join_cluster(addr_b);
    pump_until_converged(&mut [&mut rt_a, &mut rt_b], 2, Duration::from_secs(30));

    // Remote spawn, exactly like the language's `spawn@node` lowering.
    let request_id = {
        let mut transport = rt_a.distributed.transport.take().unwrap();
        let cluster = rt_a.distributed.cluster.take().unwrap();
        let resolver = rt_a.distributed.resolver.take().unwrap();
        let placeholder = spawn_on_node(
            &mut rt_a,
            &mut transport,
            &cluster,
            &resolver,
            node_b,
            "store",
            vec![("received".to_string(), Value::int(0))],
        );
        rt_a.distributed.transport = Some(transport);
        rt_a.distributed.cluster = Some(cluster);
        rt_a.distributed.resolver = Some(resolver);
        placeholder.actor_id()
    };

    // The placeholder is tagged so messages queue until the response.
    assert!(
        rt_a.spawn_placeholders.contains(&request_id),
        "spawn placeholder must be tagged pending"
    );

    let deadline = Instant::now() + Duration::from_secs(30);
    let remote_actor = loop {
        rt_a.process_network();
        rt_b.process_network();
        if let Some(result) = rt_a.take_spawn_response(request_id) {
            break result.expect("node B rejected the spawn request");
        }
        assert!(
            Instant::now() < deadline,
            "no SpawnResponse received from node B"
        );
        sleep(Duration::from_millis(50));
    };

    // The real id is now routable BY VALUE: node A recorded the bare id →
    // node mapping, and a plain `send_message(id, name, args)` (no
    // ActorAddress wrapper) must go over the wire.
    assert_eq!(
        rt_a.remote_refs.get(&remote_actor),
        Some(&node_b),
        "real remote actor id must be recorded in the reverse index"
    );

    rt_a.send_message(remote_actor, "store", &[Value::int(7)]);

    let deadline = Instant::now() + Duration::from_secs(30);
    let delivered = loop {
        rt_a.process_network();
        rt_b.process_network();
        rt_b.run_scheduler();
        let got = rt_b
            .actors
            .get(&remote_actor)
            .and_then(|a| a.get_state_field("received"))
            .and_then(|v| v.as_int());
        if got == Some(7) {
            break true;
        }
        if Instant::now() >= deadline {
            break false;
        }
        sleep(Duration::from_millis(50));
    };
    assert!(
        delivered,
        "bare-id send to the remotely-spawned actor was not delivered"
    );

    shutdown_nodes(&mut [&mut rt_a, &mut rt_b]);
}

#[cfg(feature = "tcp")]
/// RFC-0007 placeholder queue: a message sent to the spawn@node
/// placeholder BEFORE the SpawnResponse arrives is queued in wire form
/// and flushed to the real actor id on arrival — no message loss in the
/// spawn-in-flight window. After the response, the same placeholder
/// value translates to the real id and sends directly.
#[test]
fn test_remote_ref_pending_spawn_queue_flushes() {
    let mut rt_a = start_distributed_node();
    let mut rt_b = start_distributed_node();

    let addr_b = rt_b.distributed.transport.as_ref().unwrap().listen_addr();
    let node_b = rt_b.distributed.node_id.unwrap();

    rt_b.register_spawnable_behavior("store", remote_spawn_store_handler);

    rt_a.join_cluster(addr_b);
    pump_until_converged(&mut [&mut rt_a, &mut rt_b], 2, Duration::from_secs(30));

    let placeholder_id = {
        let mut transport = rt_a.distributed.transport.take().unwrap();
        let cluster = rt_a.distributed.cluster.take().unwrap();
        let resolver = rt_a.distributed.resolver.take().unwrap();
        let placeholder = spawn_on_node(
            &mut rt_a,
            &mut transport,
            &cluster,
            &resolver,
            node_b,
            "store",
            vec![("received".to_string(), Value::int(0))],
        );
        rt_a.distributed.transport = Some(transport);
        rt_a.distributed.cluster = Some(cluster);
        rt_a.distributed.resolver = Some(resolver);
        placeholder.actor_id()
    };

    // Send to the placeholder WITHOUT pumping the network first — the
    // SpawnResponse cannot have arrived, so this must queue.
    rt_a.send_message(placeholder_id, "store", &[Value::int(5)]);
    assert_eq!(
        rt_a.pending_spawn_messages
            .get(&placeholder_id)
            .map(Vec::len),
        Some(1),
        "pre-response send must be queued against the placeholder"
    );

    // Pump until the response arrives; the flush delivers the queued msg.
    let deadline = Instant::now() + Duration::from_secs(30);
    let remote_actor = loop {
        rt_a.process_network();
        rt_b.process_network();
        if let Some(result) = rt_a.take_spawn_response(placeholder_id) {
            break result.expect("node B rejected the spawn request");
        }
        if Instant::now() >= deadline {
            panic!("no SpawnResponse received from node B");
        }
        sleep(Duration::from_millis(50));
    };

    assert!(
        rt_a.pending_spawn_messages.get(&placeholder_id).is_none(),
        "queued messages must be flushed (and removed) on SpawnResponse"
    );

    // The flushed wire packet may still be in flight when the response
    // arrived; keep pumping until the handler observes the value.
    let deadline = Instant::now() + Duration::from_secs(30);
    let delivered = loop {
        rt_a.process_network();
        rt_b.process_network();
        rt_b.run_scheduler();
        let got = rt_b
            .actors
            .get(&remote_actor)
            .and_then(|a| a.get_state_field("received"))
            .and_then(|v| v.as_int());
        if got == Some(5) {
            break true;
        }
        if Instant::now() >= deadline {
            break false;
        }
        sleep(Duration::from_millis(50));
    };
    assert!(
        delivered,
        "queued pre-response message must be delivered to the spawned actor"
    );

    // After the response the placeholder VALUE translates to the real id:
    // a send through the placeholder id must route directly.
    rt_a.send_message(placeholder_id, "store", &[Value::int(6)]);
    let deadline = Instant::now() + Duration::from_secs(30);
    let delivered = loop {
        rt_a.process_network();
        rt_b.process_network();
        rt_b.run_scheduler();
        let got = rt_b
            .actors
            .get(&remote_actor)
            .and_then(|a| a.get_state_field("received"))
            .and_then(|v| v.as_int());
        if got == Some(6) {
            break true;
        }
        if Instant::now() >= deadline {
            break false;
        }
        sleep(Duration::from_millis(50));
    };
    assert!(
        delivered,
        "placeholder send after SpawnResponse must translate to the real id"
    );

    shutdown_nodes(&mut [&mut rt_a, &mut rt_b]);
}

#[cfg(feature = "tcp")]
/// RFC-0007 collision guard: `fresh_actor_id` starts at 1 on EVERY node,
/// so a remote actor id can numerically equal a local actor's id. Local
/// actors must win the routing decision — a bare-id send to a colliding
/// local actor must never hijack to the remote node.
#[test]
fn test_remote_ref_local_collision_prefers_local() {
    let mut rt_a = start_distributed_node();
    let node_b = NodeId(4242);

    // Local actor (id from the global counter — never assume a value;
    // `fresh_actor_id` is process-global, so later tests see higher ids).
    let local_id = rt_a.spawn_actor(Box::new(|| vec![]));

    // Simulate a colliding remote ref known to node B (e.g. an inbound
    // sender whose id collides with our local actor).
    rt_a.remote_refs.insert(local_id, node_b);

    // A bare-id send to the colliding id must stay LOCAL.
    rt_a.send_message(local_id, "whatever", &[Value::int(9)]);
    assert!(
        !rt_a.actors[&local_id].mailbox.is_empty(),
        "colliding bare-id send must deliver to the LOCAL actor"
    );
    assert!(
        rt_a.pending_spawn_messages.is_empty(),
        "colliding send must not be treated as a spawn placeholder"
    );

    // The remote mapping survives for genuinely-remote ids.
    assert_eq!(rt_a.remote_refs.get(&local_id), Some(&node_b));

    shutdown_nodes(&mut [&mut rt_a]);
}

// ========================================================================
// CRDT delta-sync round schedule tests
// ========================================================================

/// `sync_crdts` round schedule: round 1 and every
/// `CRDT_FULL_SYNC_INTERVAL`-th round thereafter ship full state; all
/// other rounds ship deltas.
#[test]
fn test_crdt_sync_round_schedule() {
    assert!(
        crate::runtime::distribution::crdt_sync_is_full_round(1),
        "first sync must be full"
    );
    for round in 2..=CRDT_FULL_SYNC_INTERVAL {
        assert!(
            !crate::runtime::distribution::crdt_sync_is_full_round(round),
            "round {round} should ship deltas"
        );
    }
    assert!(
        crate::runtime::distribution::crdt_sync_is_full_round(CRDT_FULL_SYNC_INTERVAL + 1),
        "round after the interval must be a full repair sync"
    );
    assert!(!crate::runtime::distribution::crdt_sync_is_full_round(
        CRDT_FULL_SYNC_INTERVAL + 2
    ));
}

#[cfg(feature = "tcp")]
/// `sync_crdts` is a no-op that does not count rounds when distribution
/// is disabled; once enabled, every call counts exactly one round.
#[test]
fn test_sync_crdts_round_counting() {
    let mut rt = Runtime::new();
    rt.sync_crdts();
    assert_eq!(
        rt.crdt_sync_rounds, 0,
        "disabled runtime must not count rounds"
    );

    let mut rt = start_distributed_node();
    rt.sync_crdts();
    rt.sync_crdts();
    assert_eq!(rt.crdt_sync_rounds, 2);
    shutdown_nodes(&mut [&mut rt]);
}

#[cfg(feature = "tcp")]
/// End-to-end: CRDT changes propagate between two clustered nodes through
/// `sync_crdts`, across both the initial full-state round (which creates
/// the entry on the receiver) and subsequent delta rounds.
#[test]
fn test_sync_crdts_full_then_delta_converges_two_nodes() {
    let mut rt_a = start_distributed_node();
    let mut rt_b = start_distributed_node();

    let addr_a = rt_a.distributed.transport.as_ref().unwrap().listen_addr();
    rt_b.join_cluster(addr_a);
    pump_until_converged(&mut [&mut rt_a, &mut rt_b], 2, Duration::from_secs(30));

    let counter_value = |rt: &mut Runtime, id| {
        rt.crdt_manager
            .as_mut()
            .and_then(|m| m.get_gcounter_mut(id))
            .map(|c| c.value())
    };

    // Round 1 ships full state: a brand-new counter created on A must
    // appear on B with the right value.
    let id = rt_a.crdt_manager.as_mut().unwrap().create_gcounter().0;
    rt_a.crdt_manager
        .as_mut()
        .unwrap()
        .get_gcounter_mut(id)
        .unwrap()
        .increment();
    rt_a.sync_crdts();

    let deadline = Instant::now() + Duration::from_secs(30);
    loop {
        rt_a.process_network();
        rt_b.process_network();
        if counter_value(&mut rt_b, id) == Some(1) {
            break;
        }
        assert!(
            Instant::now() < deadline,
            "full-state CRDT sync did not converge on node B"
        );
        sleep(Duration::from_millis(50));
    }

    // Rounds 2..=16 ship deltas: further increments must still propagate.
    for expected in 2..=3u64 {
        rt_a.crdt_manager
            .as_mut()
            .unwrap()
            .get_gcounter_mut(id)
            .unwrap()
            .increment();
        rt_a.sync_crdts();
        let deadline = Instant::now() + Duration::from_secs(30);
        loop {
            rt_a.process_network();
            rt_b.process_network();
            if counter_value(&mut rt_b, id) == Some(expected) {
                break;
            }
            assert!(
                Instant::now() < deadline,
                "delta CRDT sync did not converge on node B at value {expected}"
            );
            sleep(Duration::from_millis(50));
        }
    }
    assert!(
        rt_a.crdt_sync_rounds >= 3,
        "test must have exercised at least one delta round"
    );

    shutdown_nodes(&mut [&mut rt_a, &mut rt_b]);
}

#[test]
fn test_crypto_provider_hash_bytes() {
    let rt = Runtime::new();
    let h1 = rt.hash_bytes(b"hello");
    let h2 = rt.hash_bytes(b"hello");
    assert_eq!(h1, h2, "hash should be deterministic");
    assert_ne!(
        h1,
        rt.hash_bytes(b"world"),
        "different input, different hash"
    );
}

#[cfg(feature = "tcp")]
#[test]
fn test_mutual_tls_connect_and_verify() {
    // Two nodes with the same CA can establish a TLS connection, verify
    // each other's certificate identity, and complete the NUL0 handshake.
    let (ca_pem, ca_key) = generate_test_ca();
    let (cert_a, key_a) = generate_test_leaf("node-a", &ca_key, &ca_pem);
    let (cert_b, key_b) = generate_test_leaf("node-b", &ca_key, &ca_pem);
    let mut rt_a = start_mutual_tls_node(&ca_pem, &cert_a, &key_a);
    let mut rt_b = start_mutual_tls_node(&ca_pem, &cert_b, &key_b);
    let b_node_id = rt_b.distributed.node_id.unwrap();
    let b_addr = rt_b.distributed.transport.as_ref().unwrap().listen_addr();
    // Direct connect exercises: TLS handshake, cert verification
    // (peer cert fingerprint == expected node_id), NUL0 handshake,
    // and connection registration in the pool.
    rt_a.distributed
        .transport
        .as_mut()
        .unwrap()
        .connect(b_node_id, b_addr)
        .expect("mTLS connect between nodes with same CA should succeed");
    assert_eq!(
        rt_a.distributed
            .transport
            .as_ref()
            .unwrap()
            .connection_count(),
        1,
        "connection should be registered"
    );
    shutdown_nodes(&mut [&mut rt_a, &mut rt_b]);
}

#[cfg(feature = "tcp")]
#[test]
fn test_mutual_tls_cluster_converges() {
    // Two mTLS nodes with the same CA converge via heartbeats.
    let (ca_pem, ca_key) = generate_test_ca();
    let (cert_a, key_a) = generate_test_leaf("node-a", &ca_key, &ca_pem);
    let (cert_b, key_b) = generate_test_leaf("node-b", &ca_key, &ca_pem);
    let mut rt_a = start_mutual_tls_node(&ca_pem, &cert_a, &key_a);
    let mut rt_b = start_mutual_tls_node(&ca_pem, &cert_b, &key_b);
    let b_node_id = rt_b.distributed.node_id.unwrap();
    let b_addr = rt_b.distributed.transport.as_ref().unwrap().listen_addr();
    // Pre-connect so the sender thread finds an existing connection.
    rt_a.distributed
        .transport
        .as_mut()
        .unwrap()
        .connect(b_node_id, b_addr)
        .expect("connect");
    // Register B with cert-based node ID so tick() heartbeats to the
    // correct ID, matching the pre-established connection.
    if let Some(ref mut cluster) = rt_a.distributed.cluster {
        cluster.join_cluster_with_id(b_node_id, b_addr);
    }
    pump_until_converged(&mut [&mut rt_a, &mut rt_b], 2, Duration::from_secs(15));
    shutdown_nodes(&mut [&mut rt_a, &mut rt_b]);
}
#[cfg(feature = "tcp")]
#[test]
fn test_mutual_tls_rejects_cert_identity_mismatch() {
    let (ca_pem, ca_key) = generate_test_ca();
    let (cert_a, key_a) = generate_test_leaf("node-a", &ca_key, &ca_pem);

    // Different CA — node B's cert won't be trusted by node A.
    let (ca2_pem, ca2_key) = generate_test_ca();
    let (cert_b, key_b) = generate_test_leaf("node-b", &ca2_key, &ca2_pem);

    let mut rt_a = start_mutual_tls_node(&ca_pem, &cert_a, &key_a);
    let mut rt_b = start_mutual_tls_node(&ca2_pem, &cert_b, &key_b);
    let b_addr = rt_b.distributed.transport.as_ref().unwrap().listen_addr();

    // Connection should fail: node A's TLS client rejects node B's cert
    // (cert signed by different CA). The handshake error is surfaced as
    // connect failure which `join_cluster` swallows silently —
    // verify the cluster never converges to 2 healthy nodes.
    rt_a.join_cluster(b_addr);
    sleep(Duration::from_secs(2)); // ample time for a connect attempt
    rt_a.process_network();
    rt_b.process_network();
    let a_count = rt_a
        .distributed
        .cluster
        .as_ref()
        .unwrap()
        .healthy_node_count();
    let b_count = rt_b
        .distributed
        .cluster
        .as_ref()
        .unwrap()
        .healthy_node_count();
    assert_eq!(
        a_count, 1,
        "A should never accept B's cert from a different CA"
    );
    assert_eq!(b_count, 1, "B should never see A");
    shutdown_nodes(&mut [&mut rt_a, &mut rt_b]);
}

#[cfg(feature = "tcp")]
#[test]
fn test_mutual_tls_rejects_plaintext_peer() {
    let (ca_pem, ca_key) = generate_test_ca();
    let (cert_a, key_a) = generate_test_leaf("node-a", &ca_key, &ca_pem);

    let mut rt_tls = start_mutual_tls_node(&ca_pem, &cert_a, &key_a);
    let mut rt_plain = start_distributed_node();

    let tls_addr = rt_tls.distributed.transport.as_ref().unwrap().listen_addr();
    let plain_addr = rt_plain
        .distributed
        .transport
        .as_ref()
        .unwrap()
        .listen_addr();

    // Plaintext → mTLS: plaintext node's connection attempt reaches a TLS
    // listener, which expects a TLS ClientHello — raw TCP bytes are not a
    // valid TLS handshake, so the connection is dropped.
    rt_plain.join_cluster(tls_addr);

    // mTLS → plaintext: mTLS node's TLS ClientHello reaches a raw TCP
    // listener — the plaintext listener reads it as garbage handshake bytes
    // and drops the connection.
    rt_tls.join_cluster(plain_addr);

    sleep(Duration::from_secs(2));
    rt_tls.process_network();
    rt_plain.process_network();
    assert_eq!(
        rt_tls
            .distributed
            .cluster
            .as_ref()
            .unwrap()
            .healthy_node_count(),
        1
    );
    assert_eq!(
        rt_plain
            .distributed
            .cluster
            .as_ref()
            .unwrap()
            .healthy_node_count(),
        1
    );
    shutdown_nodes(&mut [&mut rt_tls, &mut rt_plain]);
}

// -----------------------------------------------------------------------
// DST (Deterministic Simulation Testing) integration tests
// -----------------------------------------------------------------------

/// Verify that `run_scheduler_deterministic` produces the same result
/// across two runs with the same seed, confirming the deterministic
/// scheduling path.
#[test]
fn test_dst_run_scheduler_deterministic_reproducible() {
    let run_with_seed = |seed: u64| -> i64 {
        let mut rt = Runtime::new();
        rt.install_virtual_clock();
        let actor_id = rt.spawn_actor(Box::new(|| vec![("counter".to_string(), Value::int(0))]));
        {
            let actor = rt.actors.get_mut(&actor_id).unwrap();
            actor.register_behavior("inc", |actor, _args| {
                let n = actor
                    .get_state_field("counter")
                    .and_then(|v| v.as_int())
                    .unwrap_or(0);
                actor.set_state_field("counter", Value::int(n + 1));
            });
        }
        for _ in 0..5 {
            rt.send_message(actor_id, "inc", &[]);
        }
        rt.run_scheduler_deterministic(seed, 1000);
        rt.actors
            .get(&actor_id)
            .and_then(|a| a.get_state_field("counter"))
            .and_then(|v| v.as_int())
            .unwrap_or(-1)
    };

    let v1 = run_with_seed(42);
    let v2 = run_with_seed(42);
    assert_eq!(v1, v2, "same seed must produce same result");
    assert_eq!(v1, 5, "all 5 inc messages must be processed");
}

/// Verify that `DeterministicNetworkTransport` correctly delivers
/// messages between two runtimes connected in-memory (no real TCP).
#[test]
fn test_dst_deterministic_network_transport_delivers() {
    use crate::runtime::network::{DeterministicNetworkTransport, Packet};
    use crate::runtime::NetworkTransport;
    use std::collections::HashMap;
    use std::net::{IpAddr, Ipv4Addr, SocketAddr};
    use std::sync::Arc;

    let addr_a = SocketAddr::new(IpAddr::V4(Ipv4Addr::LOCALHOST), 10001);
    let addr_b = SocketAddr::new(IpAddr::V4(Ipv4Addr::LOCALHOST), 10002);

    // Share a bus so transports can route to each other
    let bus = Arc::new(parking_lot::Mutex::new(HashMap::new()));

    let mut t_a = DeterministicNetworkTransport::bind_with_bus(addr_a, bus.clone()).unwrap();
    let mut t_b = DeterministicNetworkTransport::bind_with_bus(addr_b, bus).unwrap();

    let node_a = t_a.node_id();
    let node_b = t_b.node_id();

    t_a.register_on_bus();
    t_b.register_on_bus();
    t_b.connect(node_a, addr_a).unwrap();

    let payload = Packet::Heartbeat {
        node_id: node_b,
        timestamp: 42,
    };
    t_b.send(node_a, addr_a, payload);

    let received = t_a.receive();
    assert_eq!(received.len(), 1, "a should receive b's heartbeat");
    assert_eq!(received[0].from_node, node_b);

    t_a.disconnect(node_b);
    t_b.disconnect(node_a);
    t_a.shutdown();
    t_b.shutdown();
}

/// PLAN.md Phase 1 bullet 2 (DST): timer determinism. A program whose
/// actor arms a timer (via the runtime timer wheel) must make progress
/// under `run_scheduler_deterministic` when a virtual clock is
/// installed: the scheduler advances the virtual clock to the deadline,
/// the timer fires, the message is delivered, and the run Quiesces with
/// the timer's side effect applied. Without a virtual clock the old
/// contract holds — the run Quiesces with the timer still pending.
#[test]
fn test_dst_timer_fires_under_virtual_clock() {
    use std::time::Duration;

    let run_with = |virtual_clock: bool| -> i64 {
        let mut rt = Runtime::new();
        if virtual_clock {
            rt.install_virtual_clock();
        }
        let actor_id = rt.spawn_actor(Box::new(|| vec![("fired".to_string(), Value::int(0))]));
        let behavior_id = {
            let actor = rt.actors.get_mut(&actor_id).unwrap();
            actor.register_behavior("__timer_fired", |actor, _args| {
                let n = actor
                    .get_state_field("fired")
                    .and_then(|v| v.as_int())
                    .unwrap_or(0);
                actor.set_state_field("fired", Value::int(n + 1));
            });
            // The registered behavior's table index is what the timer
            // wheel delivers (mirrors rearm_timer's behavior_id lookup).
            actor.behavior_table.len() as u16 - 1
        };

        // Arm a 50ms timer at the actor.
        rt.timer_wheel.send_after_with_context(
            Duration::from_millis(50),
            actor_id,
            behavior_id,
            vec![],
            "dst-timer-test".to_string(),
        );

        rt.run_scheduler_deterministic(0, 1000);

        rt.actors
            .get(&actor_id)
            .and_then(|a| a.get_state_field("fired"))
            .and_then(|v| v.as_int())
            .unwrap_or(-1)
    };

    // With a virtual clock the timer fires: counter reaches 1.
    assert_eq!(
        run_with(true),
        1,
        "virtual clock: armed timer must fire and deliver its message"
    );
    // Without a virtual clock the timer cannot be driven: the run
    // Quiesces with the timer still pending (fired stays 0).
    assert_eq!(
        run_with(false),
        0,
        "no virtual clock: timer must stay pending (old contract)"
    );
}

/// PLAN.md Phase 1 bullet 2 (DST): the seed-sweep invariant test — the
/// core "10⁴ seeds per commit, fails on any invariant violation"
/// deliverable, at a CI-scalable scale. `run_scheduler_deterministic`
/// executes REAL actor code (same VM/GC as production) with seed-driven
/// scheduling, so a violation here is a real runtime bug, not a
/// simulation artifact.
///
/// Scenario: a single counter actor receives `MESSAGES` increment
/// messages, each carrying a +1 (and the batch is interleaved with
/// messages to a decoy actor so the seeded scheduler has real ordering
/// choices). Invariants that must hold for EVERY seed:
///  1. Quiescence — the run terminates, never `StepLimitExceeded`
///     (deadlock/livelock signal).
///  2. AtMostOnce delivery — the counter reaches exactly `MESSAGES`,
///     never more (double-delivery) and never fewer (a lost message).
///
/// 2000 seeds × a 200-message batch runs in ~21s (measured) because
/// the deterministic path never sleeps on wall-clock; the rest of the
/// suite runs in parallel threads underneath it.
#[test]
fn test_dst_seed_sweep_at_most_once_delivery() {
    const MESSAGES: i64 = 200;
    let seeds = crate::dst::dst_seed_count(2000);

    for seed in 0..seeds {
        let mut rt = Runtime::new();
        rt.install_virtual_clock();

        let counter = rt.spawn_actor(Box::new(|| vec![("count".to_string(), Value::int(0))]));
        {
            let actor = rt.actors.get_mut(&counter).unwrap();
            actor.register_behavior("inc", |actor, args| {
                let n = actor
                    .get_state_field("count")
                    .and_then(|v| v.as_int())
                    .unwrap_or(0);
                let by = args.get(0).and_then(|v| v.as_int()).unwrap_or(0);
                actor.set_state_field("count", Value::int(n + by));
            });
        }
        // Decoy actor: receives the same messages as the counter but
        // ignores them — its mailbox stays non-empty longer, giving the
        // seeded scheduler a real choice of which actor to run next
        // (the interleaving is what the seed permutes).
        let decoy = rt.spawn_actor(Box::new(|| vec![]));
        {
            let actor = rt.actors.get_mut(&decoy).unwrap();
            actor.register_behavior("noop", |_actor, _args| {});
        }

        // Interleave: counter gets +1 messages, decoy gets the same
        // messages as no-ops. Sends are enqueued in order but the
        // deterministic scheduler picks actors by seed.
        for _ in 0..MESSAGES {
            rt.send_message(counter, "inc", &[Value::int(1)]);
            rt.send_message(decoy, "noop", &[]);
        }

        let result = rt.run_scheduler_deterministic(seed, 100_000);
        match result {
            crate::runtime::DeterministicRunResult::Quiescent { steps } => {
                assert!(
                    steps > 0,
                    "seed {seed}: run must make progress (messages enqueued)"
                );
            }
            crate::runtime::DeterministicRunResult::StepLimitExceeded { steps } => {
                panic!("seed {seed}: StepLimitExceeded after {steps} steps — possible deadlock/livelock");
            }
        }

        let count = rt
            .actors
            .get(&counter)
            .and_then(|a| a.get_state_field("count"))
            .and_then(|v| v.as_int())
            .unwrap_or(-1);
        assert_eq!(
            count, MESSAGES,
            "seed {seed}: counter must reach exactly {MESSAGES} (AtMostOnce), got {count}"
        );
    }
}

/// PLAN.md Phase 1 bullet 2 (DST): GC-during-send scenario, seed-driven.
/// Real actor heaps with real ORCA refcount deltas racing message sends.
/// A builder actor allocates nested heap arrays on its own heap (via
/// `heap.alloc`, slots holding counted refs transferred from the alloc),
/// sends each tree to a receiver with `current_actor` set so the send
/// path's `send_ref_to` bumps the in-flight foreign count, then releases
/// its local reference (`drop_local_ref` → deferred free). The receiver
/// pops the message, takes a receiver-side hold, and sums the array
/// contents — a premature free or a refcount imbalance shows up as a
/// wrong sum, a dangling read, or a double-decrement assert. A churn
/// actor allocates+frees blocks so the seeded scheduler has a real
/// choice of which actor to run next (the GC interleaving is what the
/// seed permutes); the deterministic scheduler now pumps GC on the
/// production cadence (deferred frees mid-run, foreign-ref decrements +
/// deferred retry at quiescence). Invariants for every seed:
///  1. The run quiesces (no deadlock/livelock).
///  2. The receiver's count equals the exact summed contents of every
///     array (every message delivered, every array intact at read time).
///  3. Heap hygiene: every array tree is still alive on the builder
///     (held by the receiver — nothing prematurely freed), exactly
///     `MESSAGES * (K + 1)` objects; the receiver's own heap is empty.
#[test]
fn test_dst_gc_during_send_seed_sweep() {
    use crate::runtime::heap::{ActorHeap, TypeTag};

    const MESSAGES: usize = 40;
    const K: usize = 4; // outer array elements
    const INNER: usize = 2; // elements per inner array
    let seeds = crate::dst::dst_seed_count(60);

    for seed in 0..seeds {
        let mut rt = Runtime::new();
        rt.install_virtual_clock();

        let builder = rt.spawn_actor(Box::new(|| vec![]));
        let receiver = rt.spawn_actor(Box::new(|| vec![("count".to_string(), Value::int(0))]));
        let churner = rt.spawn_actor(Box::new(|| vec![]));

        {
            let actor = rt.actors.get_mut(&churner).unwrap();
            actor.register_behavior("churn", |actor, _args| {
                // Allocate + immediately release a fresh block (bump path,
                // size-class free list): pure allocation/free churn so the
                // seeded scheduler has a real interleaving choice. Raw tag:
                // free_object only slot-scans Array/Record/Tuple, so the
                // uninitialized payload is never reinterpreted as Values.
                if let Some(p) = actor.heap.alloc(64, TypeTag::Raw) {
                    // SAFETY: p is a live allocation on this actor's heap
                    // with exactly one local reference (the alloc).
                    unsafe {
                        actor.orca_gc.drop_local_ref(&mut actor.heap, p);
                    }
                }
            });
        }
        {
            let actor = rt.actors.get_mut(&receiver).unwrap();
            actor.register_behavior("accum", |actor, args| {
                let ptr = args.get(0).and_then(|v| v.as_ptr()).expect("payload array");
                // SAFETY: the tree is alive — the sender's in-flight
                // foreign bump plus the receiver-side hold (taken on pop)
                // keep it live until this behavior reads it; header_of and
                // the slot slices are pure pointer arithmetic over the
                // uniform OrcaHeader layout. A refcount imbalance that
                // freed the tree early shows up here as a wrong tag, a
                // dangling inner pointer, or garbage element values.
                unsafe {
                    let h = &*ActorHeap::header_of(ptr);
                    assert_eq!(h.type_tag, TypeTag::Array, "payload must be an array");
                    let slots = std::slice::from_raw_parts(
                        ptr as *const Value,
                        h.payload_size / std::mem::size_of::<Value>(),
                    );
                    let mut sum = 0i64;
                    for inner in slots {
                        let ip = inner.as_ptr().expect("inner array");
                        let ih = &*ActorHeap::header_of(ip);
                        assert_eq!(ih.type_tag, TypeTag::Array, "slot must hold an array");
                        let inner_slots = std::slice::from_raw_parts(
                            ip as *const Value,
                            ih.payload_size / std::mem::size_of::<Value>(),
                        );
                        for v in inner_slots {
                            sum += v.as_int().expect("int element");
                        }
                    }
                    let n = actor
                        .get_state_field("count")
                        .and_then(|v| v.as_int())
                        .unwrap_or(0);
                    actor.set_state_field("count", Value::int(n + sum));
                }
            });
        }

        // Pre-enqueue churn so the scheduler interleaves churn with pops.
        for _ in 0..MESSAGES {
            rt.send_message(churner, "churn", &[]);
        }
        // Build + send one nested tree per message.
        for i in 0..MESSAGES {
            let outer = {
                let actor = rt.actors.get_mut(&builder).unwrap();
                let outer = actor
                    .heap
                    .alloc(K * std::mem::size_of::<Value>(), TypeTag::Array)
                    .expect("outer alloc");
                // SAFETY: fresh allocation; every slot is written below.
                // Each inner array's alloc transfers its single counted
                // reference to the slot it is stored in (the VM's
                // ArrAlloc+ArrStore+Dot pattern balances to the same net
                // state: one counted slot ref per element).
                unsafe {
                    let slots = std::slice::from_raw_parts_mut(outer as *mut Value, K);
                    for (j, slot) in slots.iter_mut().enumerate() {
                        let inner = actor
                            .heap
                            .alloc(INNER * std::mem::size_of::<Value>(), TypeTag::Array)
                            .expect("inner alloc");
                        let inner_slots =
                            std::slice::from_raw_parts_mut(inner as *mut Value, INNER);
                        for (x, islot) in inner_slots.iter_mut().enumerate() {
                            *islot = Value::int((10 * (i * K + j) + x) as i64);
                        }
                        *slot = Value::ptr(inner);
                    }
                }
                outer
            };
            // Send from the owner's context: `current_actor` gates the
            // send path's `send_ref_to` (bumps the in-flight foreign
            // count so the tree survives until the receiver pops+holds).
            rt.current_actor = Some(builder);
            rt.send_message(receiver, "accum", &[Value::ptr(outer)]);
            rt.current_actor = None;
            // The builder releases its local reference after the send;
            // the in-flight bump defers the free until the receiver's
            // decrement lands at quiescence.
            let actor = rt.actors.get_mut(&builder).unwrap();
            // SAFETY: outer is a live allocation owned by this actor with
            // exactly one local reference (the builder's).
            unsafe {
                actor.orca_gc.drop_local_ref(&mut actor.heap, outer);
            }
        }

        let result = rt.run_scheduler_deterministic(seed, 100_000);
        match result {
            crate::runtime::DeterministicRunResult::Quiescent { steps } => {
                assert!(
                    steps > 0,
                    "seed {seed}: run must make progress (messages enqueued)"
                );
            }
            crate::runtime::DeterministicRunResult::StepLimitExceeded { steps } => {
                panic!("seed {seed}: StepLimitExceeded after {steps} steps — possible deadlock/livelock");
            }
        }

        let count = rt
            .actors
            .get(&receiver)
            .and_then(|a| a.get_state_field("count"))
            .and_then(|v| v.as_int())
            .unwrap_or(-1);
        let mut expected = 0i64;
        for i in 0..MESSAGES {
            for j in 0..K {
                for x in 0..INNER {
                    expected += (10 * (i * K + j) + x) as i64;
                }
            }
        }
        assert_eq!(
            count, expected,
            "seed {seed}: every message delivered with intact array contents, got {count}"
        );
        // Heap hygiene: every tree is still alive on the builder (held by
        // the receiver — nothing prematurely freed, nothing leaked beyond
        // the held set); the receiver's own heap was never touched.
        assert_eq!(
            rt.actors.get(&builder).unwrap().heap.live_count(),
            MESSAGES * (K + 1),
            "seed {seed}: all array trees must be alive and held (no premature free, no leak)"
        );
        assert_eq!(
            rt.actors.get(&receiver).unwrap().heap.live_count(),
            1,
            "seed {seed}: receiver heap must hold only the lazily-allocated cycle-detector sentinel (one sticky Raw object), got {}",
            rt.actors.get(&receiver).unwrap().heap.live_count()
        );
    }
}

// ========================================================================
// Object Store Tests
// ========================================================================

#[test]
fn test_object_store_put_get() {
    let mut rt = Runtime::new();
    let bytes: Box<[u8]> = vec![1, 2, 3, 4, 5].into_boxed_slice();
    let id = rt.object_store.put(bytes);
    let entry = rt.object_store.get(id).unwrap();
    assert_eq!(entry.as_bytes(), &[1, 2, 3, 4, 5]);
}

#[test]
fn test_object_ref_send_same_shard_records_hold() {
    let mut rt = Runtime::new();
    let receiver = rt.spawn_actor(Box::new(|| vec![]));
    let bytes: Box<[u8]> = vec![9, 8, 7].into_boxed_slice();
    let obj_id = rt.object_store.put(bytes);

    rt.send_message_by_id(receiver, 0, &[Value::object(obj_id)]);
    rt.step_actor(receiver);

    let actor = rt.actors.get(&receiver).unwrap();
    assert!(
        actor.held_objects.contains(&obj_id),
        "receiver should hold the object ref after delivery"
    );
    // Refcount: original put (1) + delivery hold (1) = 2
    assert_eq!(rt.object_store.get(obj_id).unwrap().ref_count(), 2);
}

#[test]
fn test_object_ref_released_on_actor_exit() {
    let mut rt = Runtime::new();
    let receiver = rt.spawn_actor(Box::new(|| vec![]));
    let bytes: Box<[u8]> = vec![9, 8, 7].into_boxed_slice();
    let obj_id = rt.object_store.put(bytes);

    rt.send_message_by_id(receiver, 0, &[Value::object(obj_id)]);
    rt.step_actor(receiver);
    assert!(rt.object_store.get(obj_id).is_some());

    // Drop the unowned creator ref so that only the receiver's hold remains.
    rt.object_store.drop_ref(obj_id);

    rt.exit_actor(receiver, ExitReason::Normal);
    assert!(
        rt.object_store.get(obj_id).is_none(),
        "object should be freed when the last actor exits"
    );
}

#[test]
fn test_object_ref_cross_shard_copies_bytes() {
    let mut shards = Runtime::new_sharded(2);

    // Spawn actors until we have one on each shard with opposite parity.
    // Actor assignment is actor_id % shard_count, but the global id counter
    // may be in an arbitrary state when this test runs.
    let mut a = shards[0].spawn_actor(Box::new(|| vec![]));
    while a % 2 != 0 {
        a = shards[0].spawn_actor(Box::new(|| vec![]));
    }
    let mut b = shards[1].spawn_actor(Box::new(|| vec![]));
    while b % 2 != 1 {
        b = shards[1].spawn_actor(Box::new(|| vec![]));
    }

    // a is in shard 0 with an even id, so routing a -> b crosses to shard 1.
    let source_shard = (a % 2) as usize;
    let target_shard = (b % 2) as usize;
    assert_eq!(source_shard, 0);
    assert_eq!(target_shard, 1);

    // Put object in the source shard's store and send from a to b.
    let bytes: Box<[u8]> = vec![11, 22, 33].into_boxed_slice();
    let obj_id = shards[source_shard].object_store.put(bytes);
    shards[source_shard].current_actor = Some(a);
    shards[source_shard].send_message_by_id(b, 0, &[Value::object(obj_id)]);

    // Pump the target shard to receive the cross-shard message.
    shards[target_shard].drain_cross_shard_messages();

    assert_eq!(
        shards[target_shard].actors.get(&b).unwrap().mailbox.len(),
        1
    );

    // The received object id is local to the target store (ids are per-store
    // and may coincide by chance, so verify via bytes, not id equality).
    let received_msg = shards[target_shard]
        .actors
        .get_mut(&b)
        .unwrap()
        .mailbox
        .pop()
        .unwrap();
    let local_id = received_msg.payload[0].as_object_id().unwrap();
    assert_eq!(
        shards[target_shard]
            .object_store
            .get(local_id)
            .unwrap()
            .as_bytes(),
        &[11, 22, 33]
    );
}

// ========================================================================
// Built-in Grain effects
// ========================================================================

fn register_test_grain(rt: &mut Runtime, name: &str) {
    let mut module = CodeModule::new(name);
    let mut meta = ActorMeta::new(name);
    meta.is_virtual = true;
    module.add_actor_meta(meta);
    let grain_type = GrainType {
        module,
        default_models: vec![],
        bytecode_offsets: vec![],
        compensation_offsets: vec![],
        dehydrate_policy: DehydratePolicy::default(),
    };
    rt.grain_registry.register(name, grain_type);
}

#[test]
fn test_grain_ref_builtin_string_key() {
    let mut rt = Runtime::new();
    register_test_grain(&mut rt, "Counter");
    let constants = vec![
        Constant::String("Counter".to_string()),
        Constant::String("alpha".to_string()),
    ];
    let regs = vec![Value::string(0), Value::string(1)];
    let result = rt.perform_grain_builtin(Some("ref"), &constants, &regs);
    let expected = Value::actor_ref(grain_actor_id(&GrainId::new("Counter", "alpha")));
    assert_eq!(result, Some(expected));
}

#[test]
fn test_grain_ref_builtin_int_key() {
    let mut rt = Runtime::new();
    let constants = vec![Constant::String("User".to_string())];
    let regs = vec![Value::string(0), Value::int(42)];
    let result = rt.perform_grain_builtin(Some("ref"), &constants, &regs);
    let expected = Value::actor_ref(grain_actor_id(&GrainId::new("User", "42")));
    assert_eq!(result, Some(expected));
}

#[test]
fn test_grain_ref_builtin_unknown_key_returns_nil() {
    let mut rt = Runtime::new();
    let constants = vec![Constant::String("User".to_string())];
    let regs = vec![Value::string(0), Value::bool(true)];
    let result = rt.perform_grain_builtin(Some("ref"), &constants, &regs);
    assert_eq!(result, Some(Value::nil()));
}

#[test]
fn test_grain_prewarm_builtin_hydrates_grain() {
    let mut rt = Runtime::new();
    register_test_grain(&mut rt, "Counter");
    let constants = vec![Constant::String("Counter".to_string())];
    let regs = vec![Value::string(0), Value::string(0)];

    let grain_id = GrainId::new("Counter", "Counter");
    let stable_id = grain_actor_id(&grain_id);
    assert!(!rt.actors.contains_key(&stable_id));

    let result = rt.perform_grain_builtin(Some("prewarm"), &constants, &regs);
    assert_eq!(result, Some(Value::unit()));
    assert!(
        rt.actors.contains_key(&stable_id),
        "prewarm should hydrate the grain"
    );
}

#[test]
fn test_grain_prewarm_builtin_unknown_type_returns_nil() {
    let mut rt = Runtime::new();
    let constants = vec![Constant::String("Unknown".to_string())];
    let regs = vec![Value::string(0), Value::string(0)];
    let result = rt.perform_grain_builtin(Some("prewarm"), &constants, &regs);
    assert_eq!(result, Some(Value::nil()));
}

#[test]
fn test_grain_pin_unpin_builtin() {
    let mut rt = Runtime::new();
    register_test_grain(&mut rt, "Counter");
    let constants = vec![Constant::String("Counter".to_string())];
    let regs = vec![Value::string(0), Value::int(7)];

    let grain_id = GrainId::new("Counter", "7");
    let stable_id = grain_actor_id(&grain_id);

    assert_eq!(
        rt.perform_grain_builtin(Some("pin"), &constants, &regs),
        Some(Value::unit())
    );
    assert!(
        rt.actors.get(&stable_id).unwrap().pinned,
        "pin should set pinned flag"
    );

    assert_eq!(
        rt.perform_grain_builtin(Some("unpin"), &constants, &regs),
        Some(Value::unit())
    );
    assert!(
        !rt.actors.get(&stable_id).unwrap().pinned,
        "unpin should clear pinned flag"
    );
}

/// Build a simple virtual `Counter` grain module with one behavior
/// `Counter.inc` that increments durable `count` and returns the new value.
fn counter_grain_module() -> crate::bytecode::CodeModule {
    use crate::bytecode::{
        ActorMeta, BehaviorTableEntry, CodeModule, Constant, Instruction, OpCode,
    };

    let mut module = CodeModule::new("grain_counter");
    let field_idx = module.add_constant(Constant::String("count".to_string()));
    let one_idx = module.add_constant(Constant::Int(1));
    module.add_behavior(BehaviorTableEntry {
        name: "Counter.inc".to_string(),
        param_count: 0,
        code_offset: 0,
        local_count: 4,
        effect_mask: 0,
        compensate_offset: None,
        content_hash: None,
        source_location: None,
        parallel_branches: None,
    });
    module.emit(Instruction::new3(
        OpCode::StateGet,
        ((field_idx >> 8) & 0xFF) as u8,
        (field_idx & 0xFF) as u8,
        1,
    ));
    module.emit(Instruction::new3(
        OpCode::ConstU,
        ((one_idx >> 8) & 0xFF) as u8,
        (one_idx & 0xFF) as u8,
        2,
    ));
    module.emit(Instruction::new3(OpCode::IAdd, 1, 2, 3));
    module.emit(Instruction::new3(OpCode::StateSet, 0, 0, 3));
    module.emit(Instruction::new1(OpCode::RetVal, 3));
    module.add_actor_meta(ActorMeta {
        name: "Counter".to_string(),
        persistent: true,
        state_models: vec![("count".to_string(), crate::ast::StateModel::Durable)],
        state_defaults: vec![("count".to_string(), Constant::Int(0))],
        behavior_indices: vec![0],
        type_hash: None,
        version: 1,
        migrations: String::new(),
        is_workflow: false,
        is_agent: false,
        is_organization: false,
        is_virtual: true,
        tools: vec![],
        semantic_memory_dimensions: None,
        procedural_memory_namespace: None,
        backend: crate::ast::ActorBackendKind::Native,
        fallback_config: String::new(),
        retry_config: String::new(),
    });
    module
}

/// A grain message sent from shard 0 to a grain owned by shard 1 is routed
/// across shards and hydrates the grain on the owning shard.
#[test]
fn test_send_to_grain_cross_shard_routes_and_hydrates() {
    let module = counter_grain_module();
    let mut shards = Runtime::new_sharded(2);

    // Register the grain type on every shard so the receiving shard can
    // hydrate the grain when the cross-shard message arrives.
    for shard in &mut shards {
        shard.register_module_grains(&module);
    }

    // Find a key whose stable actor id maps to shard 1 (odd id).
    let mut key = "0".to_string();
    let mut grain_id = GrainId::new("Counter", &key);
    let mut stable_id = grain_actor_id(&grain_id);
    let mut attempts = 0;
    while (stable_id % 2) != 1 && attempts < 1000 {
        key = format!("k{}", attempts);
        grain_id = GrainId::new("Counter", &key);
        stable_id = grain_actor_id(&grain_id);
        attempts += 1;
    }
    assert_eq!(stable_id % 2, 1, "should find a key mapping to shard 1");

    // Send from shard 0 to the grain. The grain is not resident anywhere yet.
    shards[0].send_to_grain(grain_id.clone(), "inc", vec![], 0);

    // The grain should not hydrate on shard 0; it should be routed to shard 1.
    assert!(
        !shards[0].actors.contains_key(&stable_id),
        "grain should not hydrate on the sending shard"
    );
    assert!(
        !shards[1].actors.contains_key(&stable_id),
        "grain should not yet be hydrated on the receiving shard"
    );

    // Drain cross-shard messages on shard 1 and process the enqueued message.
    shards[1].drain_cross_shard_messages();
    shards[1].run_scheduler();

    assert!(
        shards[1].actors.contains_key(&stable_id),
        "grain should hydrate on shard 1"
    );
    let actor = shards[1].actors.get(&stable_id).unwrap();
    assert_eq!(
        actor.get_state_field("count").and_then(|v| v.as_int()),
        Some(1),
        "inc message should be processed on shard 1"
    );
}
