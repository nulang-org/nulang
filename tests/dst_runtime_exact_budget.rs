//! Exact-boundary regression tests for the real actor runtime's deterministic
//! scheduler. These use real actor handlers and the real timer wheel, not the
//! lightweight message-delivery simulator in `src/dst.rs`.

use std::time::Duration;

use nulang::runtime::{DeterministicRunResult, Runtime};
use nulang::vm::Value;

fn runtime_with_counter() -> (Runtime, u64) {
    let mut runtime = Runtime::new();
    runtime.install_virtual_clock();
    let actor_id = runtime.spawn_actor(Box::new(|| vec![("count".to_string(), Value::int(0))]));
    runtime
        .actors
        .get_mut(&actor_id)
        .expect("spawned counter actor")
        .register_behavior("inc", |actor, _args| {
            let count = actor
                .get_state_field("count")
                .and_then(|value| value.as_int())
                .expect("integer count");
            actor.set_state_field("count", Value::int(count + 1));
        });
    (runtime, actor_id)
}

fn count(runtime: &Runtime, actor_id: u64) -> i64 {
    runtime.actors[&actor_id]
        .get_state_field("count")
        .and_then(|value| value.as_int())
        .expect("integer counter state")
}

#[test]
fn idle_runtime_quiesces_with_zero_step_budget() {
    let mut runtime = Runtime::new();
    runtime.install_virtual_clock();

    assert_eq!(
        runtime.run_scheduler_deterministic(7, 0),
        DeterministicRunResult::Quiescent { steps: 0 }
    );
}

#[test]
fn runtime_quiesces_when_last_message_consumes_exact_budget() {
    let (mut runtime, actor_id) = runtime_with_counter();
    runtime.send_message(actor_id, "inc", &[]);

    assert_eq!(
        runtime.run_scheduler_deterministic(7, 1),
        DeterministicRunResult::Quiescent { steps: 1 }
    );
    assert_eq!(count(&runtime, actor_id), 1);
    assert!(runtime.actors[&actor_id].mailbox.is_empty());
}

#[test]
fn runtime_reports_limit_when_one_message_still_queued() {
    let (mut runtime, actor_id) = runtime_with_counter();
    runtime.send_message(actor_id, "inc", &[]);
    runtime.send_message(actor_id, "inc", &[]);

    assert_eq!(
        runtime.run_scheduler_deterministic(7, 1),
        DeterministicRunResult::StepLimitExceeded { steps: 1 }
    );
    assert_eq!(count(&runtime, actor_id), 1);
    assert_eq!(runtime.actors[&actor_id].mailbox.len(), 1);
}

#[test]
fn zero_budget_does_not_execute_queued_actor() {
    let (mut runtime, actor_id) = runtime_with_counter();
    runtime.send_message(actor_id, "inc", &[]);

    assert_eq!(
        runtime.run_scheduler_deterministic(7, 0),
        DeterministicRunResult::StepLimitExceeded { steps: 0 }
    );
    assert_eq!(count(&runtime, actor_id), 0);
    assert_eq!(runtime.actors[&actor_id].mailbox.len(), 1);
}

#[test]
fn zero_budget_does_not_advance_virtual_time_or_fire_timer() {
    let (mut runtime, actor_id) = runtime_with_counter();
    let start = runtime.now();
    let behavior_id = runtime.actors[&actor_id].behavior_id("inc").unwrap();
    runtime
        .timer_wheel
        .send_after(Duration::from_secs(60), actor_id, behavior_id, vec![]);

    assert_eq!(
        runtime.run_scheduler_deterministic(7, 0),
        DeterministicRunResult::StepLimitExceeded { steps: 0 }
    );
    assert_eq!(runtime.now(), start);
    assert_eq!(runtime.timer_wheel.len(), 1);
    assert_eq!(count(&runtime, actor_id), 0);
}

#[test]
fn virtual_timer_and_message_quiesce_at_exact_two_step_budget() {
    let (mut runtime, actor_id) = runtime_with_counter();
    let behavior_id = runtime.actors[&actor_id].behavior_id("inc").unwrap();
    runtime
        .timer_wheel
        .send_after(Duration::from_secs(60), actor_id, behavior_id, vec![]);

    assert_eq!(
        runtime.run_scheduler_deterministic(7, 2),
        DeterministicRunResult::Quiescent { steps: 2 }
    );
    assert_eq!(count(&runtime, actor_id), 1);
    assert_eq!(runtime.timer_wheel.len(), 0);
}

#[test]
fn timer_advance_uses_budget_and_cannot_execute_handler_without_next_step() {
    let (mut runtime, actor_id) = runtime_with_counter();
    let behavior_id = runtime.actors[&actor_id].behavior_id("inc").unwrap();
    runtime
        .timer_wheel
        .send_after(Duration::from_secs(60), actor_id, behavior_id, vec![]);

    assert_eq!(
        runtime.run_scheduler_deterministic(7, 1),
        DeterministicRunResult::StepLimitExceeded { steps: 1 }
    );
    assert_eq!(count(&runtime, actor_id), 0);
    assert_eq!(runtime.timer_wheel.len(), 1);
}
