//! Versioned transport-neutral atomic durable transition protocol.

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;
    use std::collections::BTreeMap;

    fn transition() -> DurableTransition {
        DurableTransition {
            protocol: DURABLE_TRANSITION_PROTOCOL_VERSION.into(),
            owner_id: DurableOwnerId::new("tenant/orders/order-42"),
            activation_epoch: 7,
            sequence: 12,
            expected_previous_sequence: 11,
            state: Some(DurableStateCheckpoint {
                fields: BTreeMap::from([
                    ("step_index".into(), json!(2)),
                    ("order_id".into(), json!("42")),
                ]),
            }),
            workflow_events: vec![DurableWorkflowEvent::StepCompleted {
                step_name: "charge".into(),
            }],
            timers: vec![DurableTimerMutation::Set {
                timer_id: "shipping-timeout".into(),
                due_at_unix_ms: 1_800_000_000_000,
            }],
            durable_effects: vec![DurableEffectMutation::Prepared {
                effect_id: "eff-01".into(),
                operation: "Payment.charge".into(),
                request_digest: "blake3:aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa".into(),
                idempotency_key: Some("eff-01".into()),
            }],
            outbox: vec![DurableOutboxMessage {
                destination: DurableOwnerId::new("tenant/orders/notifications"),
                ordinal: 0,
                message_type: "OrderCharged".into(),
                payload: json!({"order_id":"42"}),
            }],
        }
    }

    #[test]
    fn transition_roundtrips_with_stable_tagged_records() {
        let value = serde_json::to_value(transition()).unwrap();

        assert_eq!(value["protocol"], DURABLE_TRANSITION_PROTOCOL_VERSION);
        assert_eq!(value["workflow_events"][0]["kind"], "step_completed");
        assert_eq!(value["timers"][0]["kind"], "set");
        assert_eq!(value["durable_effects"][0]["kind"], "prepared");

        let decoded: DurableTransition = serde_json::from_value(value).unwrap();
        assert_eq!(decoded, transition());
    }

    #[test]
    fn transition_validation_enforces_epoch_and_sequence_fencing() {
        let mut invalid = transition();
        invalid.activation_epoch = 0;
        assert_eq!(
            invalid.validate().unwrap_err(),
            DurableProtocolError::InvalidActivationEpoch
        );

        let mut invalid = transition();
        invalid.sequence = 13;
        assert_eq!(
            invalid.validate().unwrap_err(),
            DurableProtocolError::NonContiguousSequence {
                expected_previous_sequence: 11,
                sequence: 13,
            }
        );
    }

    #[test]
    fn deterministic_digest_ignores_state_insertion_order() {
        let mut first = transition();
        let mut second = transition();

        first.state = Some(DurableStateCheckpoint {
            fields: BTreeMap::from([
                ("a".into(), json!(1)),
                ("b".into(), json!(2)),
            ]),
        });
        second.state = Some(DurableStateCheckpoint {
            fields: BTreeMap::from([
                ("b".into(), json!(2)),
                ("a".into(), json!(1)),
            ]),
        });

        assert_eq!(first.digest().unwrap(), second.digest().unwrap());
    }

    #[test]
    fn exact_duplicate_commit_can_be_identified_by_sequence_and_digest() {
        let first = DurableCommitRequest::new(transition()).unwrap();
        let second = DurableCommitRequest::new(transition()).unwrap();

        assert_eq!(first.transition.sequence, second.transition.sequence);
        assert_eq!(first.digest, second.digest);
    }

    #[test]
    fn same_sequence_with_different_content_has_different_digest() {
        let first = DurableCommitRequest::new(transition()).unwrap();
        let mut changed = transition();
        changed.workflow_events = vec![DurableWorkflowEvent::StepFailed {
            step_name: "charge".into(),
            error: "declined".into(),
        }];
        let second = DurableCommitRequest::new(changed).unwrap();

        assert_eq!(first.transition.sequence, second.transition.sequence);
        assert_ne!(first.digest, second.digest);
    }

    #[test]
    fn timer_and_signal_records_are_semantic_not_host_specific() {
        let records = vec![
            DurableWorkflowEvent::SignalAccepted {
                name: "approved".into(),
                payload: Some(json!({"by":"manager"})),
            },
            DurableWorkflowEvent::SagaCompensated {
                step_name: "reserve".into(),
            },
            DurableWorkflowEvent::ParallelBranchCompleted {
                step_name: "notify".into(),
                branch_name: "email".into(),
            },
        ];
        let encoded = serde_json::to_value(records).unwrap();

        assert_eq!(encoded[0]["kind"], "signal_accepted");
        assert_eq!(encoded[1]["kind"], "saga_compensated");
        assert_eq!(encoded[2]["kind"], "parallel_branch_completed");
    }
}
