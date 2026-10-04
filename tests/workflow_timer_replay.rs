use std::collections::HashMap;

use nulang::runtime::{Runtime, WorkflowActivationId, WorkflowEvent, WorkflowReplayEventId};
use nulang::vm::Value;

#[test]
fn timer_set_replay_consumes_matching_committed_preparation() {
    let mut rt = Runtime::new();
    let actor_id = rt.spawn_workflow_actor("ReplayTimer", Box::new(Vec::new), HashMap::new());
    let safe_snapshot = rt.persistence.load_snapshot(actor_id).unwrap();
    let activation = WorkflowActivationId::new(actor_id, 90);

    {
        let actor = rt.actors.get_mut(&actor_id).unwrap();
        actor.current_workflow_activation = Some(activation);
        actor.workflow_replay_activation = Some(activation);
        actor.workflow_replay_event_ordinal = 0;
    }

    rt.schedule_workflow_timer(actor_id, "wake", 250).unwrap();

    let timer_sets: Vec<_> = rt
        .persistence
        .read_timer_events(actor_id)
        .into_iter()
        .filter(|event| matches!(event, WorkflowEvent::TimerSet { .. }))
        .collect();
    assert_eq!(timer_sets.len(), 1);
    assert_eq!(
        timer_sets[0].replay_id(),
        Some(WorkflowReplayEventId::new(activation, 0)),
        "timer preparation inside an activation must receive deterministic replay identity"
    );
    assert_eq!(
        rt.persistence.load_snapshot(actor_id).unwrap().sequence,
        safe_snapshot.sequence,
        "timer preparation inside an open activation must keep the last completed snapshot safe"
    );
    assert_eq!(rt.timer_wheel.len(), 1);

    {
        let actor = rt.actors.get_mut(&actor_id).unwrap();
        actor.current_workflow_activation = Some(activation);
        actor.workflow_replay_activation = Some(activation);
        actor.workflow_replay_event_ordinal = 0;
    }

    rt.schedule_workflow_timer(actor_id, "wake", 250).unwrap();

    assert_eq!(
        rt.persistence
            .read_timer_events(actor_id)
            .into_iter()
            .filter(|event| matches!(event, WorkflowEvent::TimerSet { .. }))
            .count(),
        1,
        "replay must consume the committed timer preparation instead of appending a duplicate"
    );
    assert_eq!(
        rt.timer_wheel.len(),
        1,
        "consuming a committed timer preparation must not arm a duplicate live timer"
    );
    assert_eq!(
        rt.actors
            .get(&actor_id)
            .unwrap()
            .workflow_replay_event_ordinal,
        1,
        "the shared activation-local replay cursor must advance after consuming TimerSet"
    );
}

#[test]
fn timer_set_replay_rejects_cross_type_identity_collision() {
    let mut rt = Runtime::new();
    let actor_id =
        rt.spawn_workflow_actor("ReplayTimerConflict", Box::new(Vec::new), HashMap::new());
    let activation = WorkflowActivationId::new(actor_id, 91);

    {
        let actor = rt.actors.get_mut(&actor_id).unwrap();
        actor.current_workflow_activation = Some(activation);
        actor.workflow_replay_activation = Some(activation);
        actor.workflow_replay_event_ordinal = 0;
    }
    rt.emit_event(actor_id, "AlreadyCommitted", &[Value::int(1)]);

    {
        let actor = rt.actors.get_mut(&actor_id).unwrap();
        actor.current_workflow_activation = Some(activation);
        actor.workflow_replay_activation = Some(activation);
        actor.workflow_replay_event_ordinal = 0;
    }

    let error = rt
        .schedule_workflow_timer(actor_id, "wake", 250)
        .expect_err("TimerSet must not reuse a replay id already committed by another event kind");
    assert_eq!(error.kind(), std::io::ErrorKind::InvalidData);
    assert!(
        rt.persistence.read_timer_events(actor_id).is_empty(),
        "a replay identity conflict must not append timer history"
    );
    assert!(
        rt.timer_wheel.is_empty(),
        "a replay identity conflict must not arm a live timer"
    );
    assert_eq!(
        rt.actors
            .get(&actor_id)
            .unwrap()
            .workflow_replay_event_ordinal,
        0,
        "a conflicting replay identity must not advance the cursor"
    );
}
