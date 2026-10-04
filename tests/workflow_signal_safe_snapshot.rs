use std::collections::HashMap;

use nulang::runtime::{Runtime, WorkflowActivationId, WorkflowEvent};

#[test]
fn signal_received_during_open_activation_keeps_safe_snapshot() {
    let mut rt = Runtime::new();
    let actor_id =
        rt.spawn_workflow_actor("SignalSafeSnapshot", Box::new(Vec::new), HashMap::new());
    let safe_snapshot = rt.persistence.load_snapshot(actor_id).unwrap();
    let activation = WorkflowActivationId::new(actor_id, safe_snapshot.sequence + 1);

    rt.actors
        .get_mut(&actor_id)
        .unwrap()
        .current_workflow_activation = Some(activation);

    rt.append_signal_received(actor_id, "go", Some("resume".to_string()))
        .unwrap();

    let snapshot_after_signal = rt.persistence.load_snapshot(actor_id).unwrap();
    assert_eq!(
        snapshot_after_signal.sequence, safe_snapshot.sequence,
        "signal delivery during an open activation must keep the last completed snapshot unchanged"
    );

    let matching = rt
        .persistence
        .read_workflow_events(actor_id)
        .into_iter()
        .filter(|event| {
            matches!(
                event,
                WorkflowEvent::SignalReceived { name, payload, .. }
                    if name == "go" && payload.as_deref() == Some("resume")
            )
        })
        .count();
    assert_eq!(matching, 1, "SignalReceived must still be durably recorded");
}

#[test]
fn signal_received_outside_activation_still_checkpoints() {
    let mut rt = Runtime::new();
    let actor_id = rt.spawn_workflow_actor("SignalCheckpoint", Box::new(Vec::new), HashMap::new());
    let before = rt.persistence.load_snapshot(actor_id).unwrap();

    rt.append_signal_received(actor_id, "go", None).unwrap();

    let after = rt.persistence.load_snapshot(actor_id).unwrap();
    assert!(
        after.sequence > before.sequence,
        "signal delivery outside an activation must preserve existing checkpoint behavior"
    );
}
