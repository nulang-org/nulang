use nulang::runtime::{PersistedValue, WorkflowActivationId, WorkflowEvent, WorkflowOperationId};

#[test]
fn workflow_operation_identity_is_activation_plus_ordinal() {
    let activation = WorkflowActivationId::new(42, 7);
    let first = WorkflowOperationId::new(activation, 0);
    let second = WorkflowOperationId::new(activation, 1);

    assert_eq!(first.activation, activation);
    assert_eq!(first.ordinal, 0);
    assert_ne!(first, second);
}

#[test]
fn workflow_operation_identity_derives_stable_durable_effect_ids() {
    let operation = WorkflowOperationId::new(WorkflowActivationId::new(42, 7), 3);

    let first = operation.durable_effect_id("Provider.ask");
    let replay = operation.durable_effect_id("Provider.ask");
    let next = WorkflowOperationId::new(operation.activation, 4)
        .durable_effect_id("Provider.ask");

    assert_eq!(first, replay);
    assert_ne!(first, next);
}

#[test]
fn replay_sensitive_workflow_events_expose_operation_identity() {
    let operation = WorkflowOperationId::new(WorkflowActivationId::new(42, 7), 0);
    let custom = WorkflowEvent::Custom {
        sequence: 8,
        operation: Some(operation),
        name: "audit".into(),
        args: vec![PersistedValue::Int(1)],
    };
    let timer = WorkflowEvent::TimerSet {
        sequence: 9,
        operation: Some(operation),
        name: "retry".into(),
        duration_ms: 100,
    };

    assert_eq!(custom.operation_id(), Some(operation));
    assert_eq!(timer.operation_id(), Some(operation));
}

#[test]
fn legacy_replay_sensitive_events_without_operation_identity_remain_readable() {
    let json = r#"{
        "tag":"Custom",
        "value":{"sequence":8,"name":"audit","args":[]}
    }"#;

    let event: WorkflowEvent = serde_json::from_str(json).unwrap();

    assert_eq!(event.operation_id(), None);
    assert!(matches!(
        event,
        WorkflowEvent::Custom {
            operation: None,
            ref name,
            ..
        } if name == "audit"
    ));
}
