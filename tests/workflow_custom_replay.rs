use std::collections::HashMap;

use nulang::runtime::{Runtime, WorkflowActivationId, WorkflowEvent};
use nulang::vm::Value;

#[test]
fn custom_workflow_replay_consumes_matching_committed_event() {
    let mut rt = Runtime::new();
    let actor_id = rt.spawn_workflow_actor("ReplayConsume", Box::new(Vec::new), HashMap::new());
    assert_ne!(actor_id, 0, "workflow spawn must succeed");
    let activation = WorkflowActivationId::new(actor_id, 60);

    rt.actors
        .get_mut(&actor_id)
        .unwrap()
        .current_workflow_activation = Some(activation);
    rt.emit_event(actor_id, "Committed", &[Value::int(7)]);

    assert_eq!(
        rt.persistence
            .read_workflow_events(actor_id)
            .into_iter()
            .filter(|event| matches!(event, WorkflowEvent::Custom { .. }))
            .count(),
        1
    );

    {
        let actor = rt.actors.get_mut(&actor_id).unwrap();
        actor.current_workflow_activation = Some(activation);
        actor.workflow_replay_activation = Some(activation);
        actor.workflow_replay_event_ordinal = 0;
    }

    rt.emit_event(actor_id, "Committed", &[Value::int(7)]);

    let custom: Vec<_> = rt
        .persistence
        .read_workflow_events(actor_id)
        .into_iter()
        .filter(|event| matches!(event, WorkflowEvent::Custom { .. }))
        .collect();
    assert_eq!(
        custom.len(),
        1,
        "replay must consume the committed event instead of appending a duplicate"
    );
    assert_eq!(
        rt.actors
            .get(&actor_id)
            .unwrap()
            .workflow_replay_event_ordinal,
        1,
        "consuming an exact replay match must advance the activation-local cursor"
    );
}

#[test]
fn custom_workflow_replay_rejects_conflicting_committed_event_identity() {
    let mut rt = Runtime::new();
    let actor_id = rt.spawn_workflow_actor("ReplayConflict", Box::new(Vec::new), HashMap::new());
    assert_ne!(actor_id, 0, "workflow spawn must succeed");
    let activation = WorkflowActivationId::new(actor_id, 70);

    rt.actors
        .get_mut(&actor_id)
        .unwrap()
        .current_workflow_activation = Some(activation);
    rt.emit_event(actor_id, "Original", &[Value::int(1)]);

    {
        let actor = rt.actors.get_mut(&actor_id).unwrap();
        actor.current_workflow_activation = Some(activation);
        actor.workflow_replay_activation = Some(activation);
        actor.workflow_replay_event_ordinal = 0;
    }

    rt.emit_event(actor_id, "Different", &[Value::int(2)]);

    let custom: Vec<_> = rt
        .persistence
        .read_workflow_events(actor_id)
        .into_iter()
        .filter(|event| matches!(event, WorkflowEvent::Custom { .. }))
        .collect();
    assert_eq!(
        custom.len(),
        1,
        "a conflicting replay identity must not mutate durable history"
    );
    assert_eq!(
        rt.actors
            .get(&actor_id)
            .unwrap()
            .workflow_replay_event_ordinal,
        0,
        "a conflicting committed event must not be consumed"
    );
}
