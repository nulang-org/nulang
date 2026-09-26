use super::{DurableTransition, PersistedValue, WorkflowActivationId, WorkflowEvent};
use nulang_durable_protocol as wire;
use serde_json::{json, Value};
use std::collections::BTreeMap;
use std::io;

#[derive(Debug, Clone)]
pub(crate) struct StagedDurableCommit {
    pub(crate) request: wire::DurableCommitRequest,
    pub(crate) bytes: Vec<u8>,
}

pub(crate) fn stage_commit_request(
    owner_id: &str,
    transition: &DurableTransition,
) -> io::Result<StagedDurableCommit> {
    transition.validate_structure()?;
    if owner_id.trim().is_empty() {
        return Err(invalid_input("durable wire owner id must not be empty"));
    }

    if !transition.domain_events.is_empty() {
        return Err(unsupported(
            "domain events do not yet have a lossless runtime-to-wire mapping",
        ));
    }
    if !transition.durable_effects.is_empty() {
        return Err(unsupported(
            "durable effects do not yet have a lossless runtime-to-wire mapping",
        ));
    }
    if !transition.outbox.is_empty() {
        return Err(unsupported(
            "outbox messages do not yet have a lossless runtime-to-wire mapping",
        ));
    }

    let command = transition
        .command
        .as_ref()
        .map(|command| {
            let args = command
                .payload
                .iter()
                .map(persisted_value_to_wire)
                .collect::<io::Result<Vec<_>>>()?;
            Ok(wire::DurableCommand {
                command_id: format!("{owner_id}:command:{}", command.sequence),
                command_type: "actor_behavior".into(),
                payload: json!({
                    "behavior_id": command.behavior_id,
                    "args": args,
                }),
            })
        })
        .transpose()?;

    let state = transition
        .snapshot
        .as_ref()
        .map(|snapshot| {
            if snapshot.waiting_signal.is_some()
                || snapshot.crdt_snapshot.is_some()
                || snapshot.crdt_field_map.is_some()
                || !snapshot.authority_tokens.is_empty()
            {
                return Err(unsupported(
                    "snapshot suspension/CRDT/authority metadata has no durable wire mapping yet",
                ));
            }

            let fields = snapshot
                .state
                .iter()
                .map(|(name, value)| Ok((name.clone(), persisted_value_to_wire(value)?)))
                .collect::<io::Result<BTreeMap<_, _>>>()?;
            Ok(wire::DurableStateCheckpoint { fields })
        })
        .transpose()?;

    let workflow_events = transition
        .workflow_events
        .iter()
        .map(|event| workflow_event_to_wire(event, transition))
        .collect::<io::Result<Vec<_>>>()?;

    let wire_transition = wire::DurableTransition {
        protocol: wire::DURABLE_TRANSITION_PROTOCOL_VERSION.into(),
        owner_id: wire::DurableOwnerId::new(owner_id),
        activation_epoch: transition.activation_epoch,
        sequence: transition.sequence,
        expected_previous_sequence: transition.expected_previous_sequence,
        command,
        state,
        workflow_events,
        domain_events: Vec::new(),
        timers: Vec::new(),
        durable_effects: Vec::new(),
        outbox: Vec::new(),
    };

    let request = wire::DurableCommitRequest::new(wire_transition).map_err(protocol_error)?;
    let bytes = serde_json::to_vec(&request)
        .map_err(|error| invalid_data(format!("durable commit request serialize: {error}")))?;
    Ok(StagedDurableCommit { request, bytes })
}

pub(crate) fn validate_commit_response(
    staged: &StagedDurableCommit,
    bytes: &[u8],
) -> io::Result<wire::DurableCommit> {
    let commit: wire::DurableCommit = serde_json::from_slice(bytes)
        .map_err(|error| invalid_data(format!("durable commit response decode: {error}")))?;

    let transition = &staged.request.transition;
    if commit.owner_id != transition.owner_id
        || commit.activation_epoch != transition.activation_epoch
        || commit.sequence != transition.sequence
        || commit.digest != staged.request.digest
    {
        return Err(invalid_data(
            "durable commit response does not match staged request",
        ));
    }

    Ok(commit)
}

fn workflow_event_to_wire(
    event: &WorkflowEvent,
    transition: &DurableTransition,
) -> io::Result<wire::DurableWorkflowEvent> {
    match event {
        WorkflowEvent::StepCompleted {
            activation,
            step_name,
            ..
        } => Ok(wire::DurableWorkflowEvent::StepCompleted {
            activation: Some(workflow_activation_to_wire(
                activation.as_ref(),
                transition,
            )?),
            step_name: step_name.clone(),
        }),
        WorkflowEvent::StepFailed {
            activation,
            step_name,
            error,
            ..
        } => Ok(wire::DurableWorkflowEvent::StepFailed {
            activation: Some(workflow_activation_to_wire(
                activation.as_ref(),
                transition,
            )?),
            step_name: step_name.clone(),
            error: error.clone(),
        }),
        WorkflowEvent::WorkflowStarted { .. } => Err(unsupported(
            "workflow-start state is not represented losslessly by the current wire event",
        )),
        WorkflowEvent::TimerSet { .. } | WorkflowEvent::TimerFired { .. } => Err(unsupported(
            "timer generation/due-time semantics do not yet have a lossless runtime-to-wire mapping",
        )),
        WorkflowEvent::SignalReceived { .. } => Err(unsupported(
            "signal records do not yet have a lossless runtime-to-wire mapping",
        )),
        WorkflowEvent::SagaCompensated { .. } => Err(unsupported(
            "saga compensation records do not yet have a lossless runtime-to-wire mapping",
        )),
        WorkflowEvent::ParallelBranchCompleted { .. } => Err(unsupported(
            "parallel workflow records do not yet have a lossless runtime-to-wire mapping",
        )),
        WorkflowEvent::Custom { .. } => Err(unsupported(
            "custom workflow records do not yet have a lossless runtime-to-wire mapping",
        )),
    }
}

fn workflow_activation_to_wire(
    activation: Option<&WorkflowActivationId>,
    transition: &DurableTransition,
) -> io::Result<wire::DurableWorkflowActivation> {
    let activation = activation.ok_or_else(|| {
        invalid_input("terminal workflow event is missing accepted-command activation identity")
    })?;
    let command = transition.command.as_ref().ok_or_else(|| {
        invalid_input("terminal workflow transition is missing its accepted command record")
    })?;
    if activation.actor_id != transition.actor_id || activation.command_sequence != command.sequence
    {
        return Err(invalid_input(
            "terminal workflow activation identity does not match transition command",
        ));
    }
    Ok(wire::DurableWorkflowActivation {
        actor_id: activation.actor_id,
        command_sequence: activation.command_sequence,
    })
}

fn persisted_value_to_wire(value: &PersistedValue) -> io::Result<Value> {
    Ok(match value {
        PersistedValue::Int(value) => json!({"tag":"int","value":value.to_string()}),
        PersistedValue::Float(value) => {
            json!({"tag":"float","bits":format!("{:016x}", value.to_bits())})
        }
        PersistedValue::Bool(value) => json!({"tag":"bool","value":value}),
        PersistedValue::String(value) => json!({"tag":"string","value":value}),
        PersistedValue::Nil => json!({"tag":"nil"}),
        PersistedValue::Unit => json!({"tag":"unit"}),
        PersistedValue::Actor(value) => json!({"tag":"actor","value":value.to_string()}),
    })
}

fn protocol_error(error: wire::DurableProtocolError) -> io::Error {
    invalid_data(format!("durable wire protocol error: {error}"))
}

fn invalid_input(message: impl Into<String>) -> io::Error {
    io::Error::new(io::ErrorKind::InvalidInput, message.into())
}

fn invalid_data(message: impl Into<String>) -> io::Error {
    io::Error::new(io::ErrorKind::InvalidData, message.into())
}

fn unsupported(message: impl Into<String>) -> io::Error {
    io::Error::new(io::ErrorKind::Unsupported, message.into())
}

#[cfg(test)]
mod tests {
        use super::*;
        use crate::runtime::{
            ActorSnapshot, DurableOutboxMessage, JournalEntry, DURABLE_TRANSITION_VERSION,
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
        transition.outbox.push(DurableOutboxMessage {
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

}
