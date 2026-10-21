//! Record/reproduce regression tests for the real actor scheduler.
//! Scheduling decisions are indexed by sorted ready-actor rank so fresh
//! runtimes need not reuse the process-global actor ID counter.
use super::*;
use crate::vm::Value;
use std::time::Duration;

fn two_counters(messages: usize) -> (Runtime, u64, u64) {
    let mut rt = Runtime::new();
    rt.install_virtual_clock();
    let first = rt.spawn_actor(Box::new(|| vec![("count".to_string(), Value::int(0))]));
    let second = rt.spawn_actor(Box::new(|| vec![("count".to_string(), Value::int(0))]));
    for id in [first, second] {
        rt.actors.get_mut(&id).unwrap().register_behavior("inc", |actor, _| {
            let count = actor.get_state_field("count").and_then(|v| v.as_int()).unwrap();
            actor.set_state_field("count", Value::int(count + 1));
        });
        for _ in 0..messages {
            rt.send_message(id, "inc", &[]);
        }
    }
    (rt, first, second)
}

#[test]
fn recorded_schedule_verifies_against_fresh_runtime_with_different_actor_ids() {
    let (mut source, first, _) = two_counters(4);
    let trace = source.run_scheduler_deterministic_recorded(0x5eed, 100);
    assert_eq!(trace.outcome, DeterministicRunResult::Quiescent { steps: 8 });
    assert_eq!(trace.events.len(), 8);
    assert!(trace.events.iter().all(|event| matches!(
        event,
        DeterministicScheduleEvent::ActorStep { .. }
    )));

    let (mut fresh, new_first, new_second) = two_counters(4);
    assert_ne!(first, new_first, "actor ids are process-generated");
    assert_eq!(
        fresh.verify_deterministic_trace(&trace),
        Ok(DeterministicRunResult::Quiescent { steps: 8 }),
    );
    for id in [new_first, new_second] {
        assert_eq!(
            fresh.actors[&id].get_state_field("count").and_then(|v| v.as_int()),
            Some(4)
        );
    }

    // The trace can be stored as a JSON reproduction artifact.
    let encoded = serde_json::to_string(&trace).unwrap();
    let decoded: DeterministicScheduleTrace = serde_json::from_str(&encoded).unwrap();
    assert_eq!(decoded, trace);
}

#[test]
fn corrupted_schedule_is_reported_as_mismatch_not_success() {
    let (mut source, _, _) = two_counters(3);
    let mut trace = source.run_scheduler_deterministic_recorded(7, 100);
    trace.events[0] = DeterministicScheduleEvent::ActorStep {
        ready_count: 999,
        selected_ready_index: 0,
    };

    let (mut fresh, _, _) = two_counters(3);
    assert!(matches!(
        fresh.verify_deterministic_trace(&trace),
        Err(DeterministicTraceMismatch::Event { index: 0, .. })
    ));
}

#[test]
fn changed_workload_is_not_accepted_as_matching_replay() {
    let (mut source, _, _) = two_counters(3);
    let trace = source.run_scheduler_deterministic_recorded(11, 100);
    let (mut fresh, _, _) = two_counters(2);
    assert!(fresh.verify_deterministic_trace(&trace).is_err());
}

#[test]
fn recording_captures_logical_virtual_timer_advances() {
    let mut rt = Runtime::new();
    rt.install_virtual_clock();
    let actor_id = rt.spawn_actor(Box::new(|| vec![("fired".to_string(), Value::int(0))]));
    let behavior_id = {
        let actor = rt.actors.get_mut(&actor_id).unwrap();
        actor.register_behavior("__timer_fired", |actor, _| {
            actor.set_state_field("fired", Value::int(1));
        });
        actor.behavior_table.len() as u16 - 1
    };
    rt.timer_wheel.send_after_with_context(
        Duration::from_millis(50),
        actor_id,
        behavior_id,
        vec![],
        "recorded-timer".to_string(),
    );

    let trace = rt.run_scheduler_deterministic_recorded(17, 100);
    assert_eq!(trace.outcome, DeterministicRunResult::Quiescent { steps: 2 });
    assert_eq!(
        trace.events,
        vec![
            DeterministicScheduleEvent::TimerAdvance,
            DeterministicScheduleEvent::ActorStep {
                ready_count: 1,
                selected_ready_index: 0,
            },
        ],
    );
    assert_eq!(
        rt.actors[&actor_id]
            .get_state_field("fired")
            .and_then(|v| v.as_int()),
        Some(1),
    );
}

#[test]
fn step_bound_is_part_of_reproduction_contract() {
    let (mut source, _, _) = two_counters(3);
    let trace = source.run_scheduler_deterministic_recorded(123, 2);
    assert_eq!(trace.outcome, DeterministicRunResult::StepLimitExceeded { steps: 2 });
    assert_eq!(trace.events.len(), 2);

    let (mut fresh, _, _) = two_counters(3);
    assert_eq!(fresh.verify_deterministic_trace(&trace), Ok(trace.outcome));
}
