use nulang::runtime::{WorkflowActivationId, WorkflowEvent, WorkflowReplayId};

#[test]
fn custom_workflow_event_exposes_activation_local_replay_identity() {
    let activation = WorkflowActivationId::new(42, 7);
    let replay_id = WorkflowReplayId::new(activation, 3);
    let event = WorkflowEvent::Custom {
        sequence: 11,
        replay_id: Some(replay_id),
        name: "Charged".into(),
        args: vec![],
    };

    assert_eq!(event.replay_id(), Some(replay_id));
    assert_eq!(replay_id.activation, activation);
    assert_eq!(replay_id.ordinal, 3);
}

#[test]
fn legacy_custom_event_omits_absent_replay_identity() {
    let event = WorkflowEvent::Custom {
        sequence: 11,
        replay_id: None,
        name: "Legacy".into(),
        args: vec![],
    };

    let encoded = serde_json::to_value(event).unwrap();
    let value = encoded
        .get("value")
        .and_then(serde_json::Value::as_object)
        .expect("Custom content object");

    assert!(
        !value.contains_key("replay_id"),
        "legacy custom-event encoding must remain byte-compatible"
    );
}

#[test]
fn replay_identity_changes_only_with_activation_or_ordinal() {
    let activation = WorkflowActivationId::new(42, 7);
    let first = WorkflowReplayId::new(activation, 0);
    let retry = WorkflowReplayId::new(activation, 0);
    let next = WorkflowReplayId::new(activation, 1);
    let other_activation = WorkflowReplayId::new(WorkflowActivationId::new(42, 8), 0);

    assert_eq!(first, retry);
    assert_ne!(first, next);
    assert_ne!(first, other_activation);
}
