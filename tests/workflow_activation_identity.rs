use nulang::runtime::{WorkflowActivationId, WorkflowEvent};

#[test]
fn terminal_workflow_events_carry_the_command_activation_identity() {
    let activation = WorkflowActivationId::new(42, 7);

    let completed = WorkflowEvent::StepCompleted {
        sequence: 11,
        activation: Some(activation),
        step_name: "charge".into(),
    };
    let failed = WorkflowEvent::StepFailed {
        sequence: 12,
        activation: Some(activation),
        step_name: "charge".into(),
        error: "provider error".into(),
    };

    assert_eq!(completed.activation_id(), Some(activation));
    assert_eq!(failed.activation_id(), Some(activation));
}

#[test]
fn legacy_terminal_events_without_activation_identity_remain_readable() {
    let json = r#"{
        "tag":"StepCompleted",
        "value":{"sequence":11,"step_name":"charge"}
    }"#;

    let event: WorkflowEvent = serde_json::from_str(json).unwrap();

    assert_eq!(event.activation_id(), None);
    assert!(matches!(
        event,
        WorkflowEvent::StepCompleted {
            sequence: 11,
            activation: None,
            ref step_name,
        } if step_name == "charge"
    ));
}

#[test]
fn workflow_activation_identity_round_trips_exact_command_sequence() {
    let activation = WorkflowActivationId::new(9, 1234);
    let encoded = serde_json::to_string(&activation).unwrap();
    let decoded: WorkflowActivationId = serde_json::from_str(&encoded).unwrap();

    assert_eq!(decoded.actor_id, 9);
    assert_eq!(decoded.command_sequence, 1234);
    assert_eq!(decoded, activation);
}
