use nulang::runtime::{WorkflowActivationId, WorkflowEvent, WorkflowOperationId};

#[test]
fn custom_workflow_event_exposes_activation_local_operation_identity() {
    let activation = WorkflowActivationId::new(42, 7);
    let operation = WorkflowOperationId::new(activation, 3);
    let event = WorkflowEvent::Custom {
        sequence: 11,
        operation: Some(operation),
        name: "Charged".into(),
        args: vec![],
    };

    assert_eq!(event.operation_id(), Some(operation));
    assert_eq!(operation.activation, activation);
    assert_eq!(operation.ordinal, 3);
}

#[test]
fn workflow_operation_identity_derives_stable_durable_effect_ids() {
    let activation = WorkflowActivationId::new(42, 7);
    let operation = activation.operation(3);

    let first = operation.durable_effect_id("Provider.ask");
    let replay = activation.operation(3).durable_effect_id("Provider.ask");
    let next = activation.operation(4).durable_effect_id("Provider.ask");

    assert_eq!(first, replay);
    assert_ne!(first, next);
}

#[test]
fn legacy_custom_event_omits_absent_operation_identity() {
    let event = WorkflowEvent::Custom {
        sequence: 11,
        operation: None,
        name: "Legacy".into(),
        args: vec![],
    };

    let encoded = serde_json::to_value(event).unwrap();
    let value = encoded
        .get("value")
        .and_then(serde_json::Value::as_object)
        .expect("Custom content object");

    assert!(
        !value.contains_key("operation"),
        "legacy custom-event encoding must remain byte-compatible"
    );
}

#[test]
fn operation_identity_changes_only_with_activation_or_ordinal() {
    let activation = WorkflowActivationId::new(42, 7);
    let first = WorkflowOperationId::new(activation, 0);
    let retry = WorkflowOperationId::new(activation, 0);
    let next = WorkflowOperationId::new(activation, 1);
    let other_activation = WorkflowOperationId::new(WorkflowActivationId::new(42, 8), 0);

    assert_eq!(first, retry);
    assert_ne!(first, next);
    assert_ne!(first, other_activation);
}
