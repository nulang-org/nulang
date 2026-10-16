use std::collections::HashMap;

use nulang::runtime::{Runtime, WorkflowActivationId, WorkflowEvent};

#[test]
fn signal_receipt_commits_one_snapshot_and_event_transition() {
    let mut runtime = Runtime::new();
    let actor_id =
        runtime.spawn_workflow_actor("AtomicSignal", Box::new(Vec::new), HashMap::new());

    runtime
        .append_signal_received(actor_id, "approved", Some("yes".into()))
        .expect("signal admission must commit atomically");

    let tail = runtime
        .persistence
        .load_durable_tail_position(actor_id)
        .unwrap()
        .expect("signal admission must advance the atomic tail");
    let snapshot = runtime.persistence.load_snapshot(actor_id).unwrap();

    assert_eq!(snapshot.sequence, tail.sequence);
    assert!(runtime.persistence.read_workflow_events(actor_id).iter().any(
        |event| matches!(
            event,
            WorkflowEvent::SignalReceived {
                sequence,
                name,
                payload: Some(payload)
            } if *sequence == tail.sequence && name == "approved" && payload == "yes"
        )
    ));
}

#[test]
fn intermediate_timer_uses_atomic_history_without_advancing_safe_snapshot() {
    let mut runtime = Runtime::new();
    let actor_id =
        runtime.spawn_workflow_actor("AtomicTimer", Box::new(Vec::new), HashMap::new());
    let safe_snapshot = runtime.persistence.load_snapshot(actor_id).unwrap();

    let activation = WorkflowActivationId::new(actor_id, 73);
    runtime
        .actors
        .get_mut(&actor_id)
        .unwrap()
        .current_workflow_activation = Some(activation);

    assert!(runtime.append_timer_set(actor_id, "wake", 250).unwrap());

    let tail = runtime
        .persistence
        .load_durable_tail_position(actor_id)
        .unwrap()
        .expect("timer preparation must advance the atomic tail");
    assert!(tail.sequence > safe_snapshot.sequence);
    assert_eq!(
        runtime.persistence.load_snapshot(actor_id).unwrap().sequence,
        safe_snapshot.sequence,
        "open activation must preserve the last safe snapshot"
    );
    assert!(runtime.persistence.read_workflow_events(actor_id).iter().any(
        |event| matches!(
            event,
            WorkflowEvent::TimerSet {
                sequence,
                replay_id: Some(id),
                name,
                duration_ms: 250,
            } if *sequence == tail.sequence && id.activation == activation && name == "wake"
        )
    ));
}

#[test]
fn fired_timers_and_saga_compensations_commit_atomically() {
    let mut runtime = Runtime::new();
    let actor_id =
        runtime.spawn_workflow_actor("AtomicProgress", Box::new(Vec::new), HashMap::new());

    runtime.append_timer_fired(actor_id, "wake").unwrap();
    let fired_tail = runtime
        .persistence
        .load_durable_tail_position(actor_id)
        .unwrap()
        .expect("timer fire must advance the atomic tail");
    assert_eq!(
        runtime.persistence.load_snapshot(actor_id).unwrap().sequence,
        fired_tail.sequence
    );

    runtime.append_saga_compensated(actor_id, "charge").unwrap();
    let compensated_tail = runtime
        .persistence
        .load_durable_tail_position(actor_id)
        .unwrap()
        .expect("compensation must advance the atomic tail");
    assert!(compensated_tail.sequence > fired_tail.sequence);
    assert_eq!(
        runtime.persistence.load_snapshot(actor_id).unwrap().sequence,
        compensated_tail.sequence
    );
    assert!(runtime.persistence.read_workflow_events(actor_id).iter().any(
        |event| matches!(
            event,
            WorkflowEvent::SagaCompensated {
                sequence,
                step_name
            } if *sequence == compensated_tail.sequence && step_name == "charge"
        )
    ));
}
