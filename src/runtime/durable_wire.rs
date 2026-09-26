use super::{
    ActorSnapshot, DurableTransition, JournalEntry, PersistedValue, WorkflowActivationId,
    WorkflowEvent, DURABLE_TRANSITION_VERSION,
};
use nulang_durable_protocol::{
    DurableCommit as WireDurableCommit, DurableWorkflowActivation, DurableWorkflowEvent,
};
use serde_json::json;
use std::collections::HashMap;

fn terminal_transition() -> DurableTransition {
    DurableTransition {
        version: DURABLE_TRANSITION_VERSION,
        actor_id: 42,
        activation_epoch: 3,
        sequence: 7,
        expected_previous_sequence: 6,
        command: Some(JournalEntry {
            sequence: 7,
            behavior_id: 11,
            payload: vec![
                PersistedValue::Int(9),
                PersistedValue::Unit,
                PersistedValue::Actor(77),
            ],
        }),
        snapshot: Some(ActorSnapshot {
            actor_id: 42,
            sequence: 7,
            state: HashMap::from([
                ("count".into(), PersistedValue::Int(9)),
                ("done".into(), PersistedValue::Bool(true)),
                ("result".into(), PersistedValue::Unit),
            ]),
            ..ActorSnapshot::default()
        }),
        workflow_events: vec![WorkflowEvent::StepCompleted {
            sequence: 7,
            activation: Some(WorkflowActivationId::new(42, 7)),
            step_name: "charge".into(),
        }],
        domain_events: Vec::new(),
        durable_effects: Vec::new(),
        outbox: Vec::new(),
    }
}

#[test]
fn terminal_transition_stages_losslessly_for_host_commit() {
    let staged = stage_commit_request("actor-42", &terminal_transition()).unwrap();

    assert_eq!(staged.request.transition.owner_id.as_str(), "actor-42");
    assert_eq!(staged.request.transition.activation_epoch, 3);
    assert_eq!(staged.request.transition.sequence, 7);
    assert_eq!(
        staged.request.transition.command.as_ref().unwrap().command_type,
        "actor_behavior"
    );
    assert_eq!(
        staged.request.transition.command.as_ref().unwrap().payload,
        json!({
            "behavior_id": 11,
            "args": [
                {"tag":"int","value":"9"},
                {"tag":"unit"},
                {"tag":"actor","value":"77"}
            ]
        })
    );
    assert_eq!(
        staged.request.transition.state.as_ref().unwrap().fields["result"],
        json!({"tag":"unit"})
    );
    assert!(matches!(
        &staged.request.transition.workflow_events[0],
        DurableWorkflowEvent::StepCompleted {
            activation: Some(DurableWorkflowActivation {
                actor_id: 42,
                command_sequence: 7,
            }),
            step_name,
        } if step_name == "charge"
    ));

    let decoded: nulang_durable_protocol::DurableCommitRequest =
        serde_json::from_slice(&staged.bytes).unwrap();
    assert_eq!(decoded, staged.request);
    decoded.validate().unwrap();
}

#[test]
fn unsupported_timer_semantics_fail_closed_instead_of_being_dropped() {
    let mut transition = terminal_transition();
    transition.workflow_events = vec![WorkflowEvent::TimerSet {
        sequence: 7,
        name: "retry".into(),
        duration_ms: 1000,
    }];

    assert!(stage_commit_request("actor-42", &transition).is_err());
}

#[test]
fn snapshot_metadata_without_wire_mapping_fails_closed() {
    let mut transition = terminal_transition();
    transition.snapshot.as_mut().unwrap().waiting_signal = Some("approved".into());

    assert!(stage_commit_request("actor-42", &transition).is_err());
}

#[test]
fn unsupported_atomic_records_fail_closed_instead_of_being_omitted() {
    let mut transition = terminal_transition();
    transition.outbox.push(super::DurableOutboxMessage {
        destination_actor_id: 99,
        ordinal: 0,
        behavior_id: 5,
        payload: vec![PersistedValue::String("hello".into())],
    });

    assert!(stage_commit_request("actor-42", &transition).is_err());
}

#[test]
fn host_commit_response_must_match_the_staged_request() {
    let staged = stage_commit_request("actor-42", &terminal_transition()).unwrap();
    let valid = WireDurableCommit {
        owner_id: staged.request.transition.owner_id.clone(),
        activation_epoch: staged.request.transition.activation_epoch,
        sequence: staged.request.transition.sequence,
        digest: staged.request.digest.clone(),
    };
    let encoded = serde_json::to_vec(&valid).unwrap();

    assert_eq!(
        validate_commit_response(&staged, &encoded).unwrap(),
        valid
    );

    let mut wrong = valid;
    wrong.sequence += 1;
    let encoded = serde_json::to_vec(&wrong).unwrap();
    assert!(validate_commit_response(&staged, &encoded).is_err());
}
