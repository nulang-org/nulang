use std::collections::HashMap;

use nulang::runtime::{Runtime, WorkflowActivationId, WorkflowEvent, WorkflowReplayEventId};
use nulang::vm::Value;

#[test]
fn replay_identified_custom_event_keeps_last_completed_snapshot_unchanged() {
    let mut rt = Runtime::new();
    let actor_id = rt.spawn_workflow_actor("ReplaySafeSnapshot", Box::new(Vec::new), HashMap::new());
    assert_ne!(actor_id, 0, "workflow spawn must succeed");

    let safe_snapshot = rt.persistence.load_snapshot(actor_id).unwrap();
    let activation = WorkflowActivationId::new(actor_id, 77);
    rt.actors
        .get_mut(&actor_id)
        .unwrap()
        .current_workflow_activation = Some(activation);

    rt.emit_event(actor_id, "CommittedBeforeCrash", &[Value::int(9)]);

    let after_event_snapshot = rt.persistence.load_snapshot(actor_id).unwrap();
    assert_eq!(
        after_event_snapshot.sequence, safe_snapshot.sequence,
        "an intermediate replay-identified event must not advance the last completed snapshot"
    );

    let matching: Vec<_> = rt
        .persistence
        .read_workflow_events(actor_id)
        .into_iter()
        .filter(|event| {
            matches!(
                event,
                WorkflowEvent::Custom {
                    replay_id: Some(id),
                    name,
                    ..
                } if *id == WorkflowReplayEventId::new(activation, 0)
                    && name == "CommittedBeforeCrash"
            )
        })
        .collect();
    assert_eq!(matching.len(), 1, "the intermediate event must still be durable");
    assert_eq!(
        rt.actors
            .get(&actor_id)
            .unwrap()
            .workflow_replay_event_ordinal,
        1,
        "durably appending the event must advance the activation-local replay cursor"
    );
}
