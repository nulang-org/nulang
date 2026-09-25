//! Versioned transport-neutral workflow runtime control protocol.

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;
    use std::collections::BTreeMap;

    #[test]
    fn test_start_command_roundtrips_with_stable_operation_name() {
        let request = WorkflowControlRequest::new(
            "req-1",
            WorkflowControlCommand::Start {
                definition_id: WorkflowDefinitionId::new("orders"),
                instance_id: Some(WorkflowInstanceId::new("order-42")),
                input: BTreeMap::from([("order_id".into(), json!(42))]),
                idempotency_key: Some("start-order-42".into()),
            },
        );

        let value = serde_json::to_value(&request).unwrap();
        assert_eq!(value["protocol"], WORKFLOW_CONTROL_PROTOCOL_VERSION);
        assert_eq!(value["command"]["operation"], "start");
        assert_eq!(value["command"]["definition_id"], "orders");

        let decoded: WorkflowControlRequest = serde_json::from_value(value).unwrap();
        assert_eq!(decoded, request);
    }

    #[test]
    fn test_lifecycle_commands_are_runtime_transport_neutral() {
        let instance_id = WorkflowInstanceId::new("wf-1");
        let commands = [
            WorkflowControlCommand::Inspect {
                instance_id: instance_id.clone(),
            },
            WorkflowControlCommand::Signal {
                instance_id: instance_id.clone(),
                signal: "approved".into(),
                payload: json!({"by": "manager"}),
            },
            WorkflowControlCommand::Cancel {
                instance_id: instance_id.clone(),
                reason: Some("customer_request".into()),
            },
            WorkflowControlCommand::Query {
                instance_id,
                query: "status".into(),
                args: vec![],
            },
        ];

        let operations: Vec<String> = commands
            .iter()
            .map(|command| {
                serde_json::to_value(command).unwrap()["operation"]
                    .as_str()
                    .unwrap()
                    .to_owned()
            })
            .collect();

        assert_eq!(operations, ["inspect", "signal", "cancel", "query"]);
    }

    #[test]
    fn test_snapshot_and_query_reply_roundtrip() {
        let snapshot = WorkflowSnapshot {
            instance_id: WorkflowInstanceId::new("wf-1"),
            definition_id: WorkflowDefinitionId::new("orders"),
            status: WorkflowLifecycleStatus::Waiting,
            current_step: Some("await_approval".into()),
            waiting_on: Some(WorkflowWait::Signal {
                name: "approved".into(),
            }),
            output: None,
            failure: None,
        };
        let response = WorkflowControlResponse::ok(
            "req-2",
            WorkflowControlReply::Inspected {
                workflow: snapshot.clone(),
            },
        );

        let encoded = serde_json::to_string(&response).unwrap();
        let decoded: WorkflowControlResponse = serde_json::from_str(&encoded).unwrap();
        assert_eq!(decoded, response);

        let query = WorkflowControlResponse::ok(
            "req-3",
            WorkflowControlReply::QueryResult {
                value: json!({"ready": true}),
            },
        );
        assert_eq!(
            serde_json::to_value(query).unwrap()["result"]["reply"]["kind"],
            "query_result"
        );
    }

    #[test]
    fn test_protocol_version_validation_rejects_mismatch() {
        assert!(validate_protocol_version(WORKFLOW_CONTROL_PROTOCOL_VERSION).is_ok());
        assert_eq!(
            validate_protocol_version("nulang-workflow-control/v999").unwrap_err(),
            WorkflowProtocolError::UnsupportedVersion(
                "nulang-workflow-control/v999".into()
            )
        );
    }

    #[test]
    fn test_error_outcome_preserves_retryability() {
        let response = WorkflowControlResponse::error(
            "req-4",
            WorkflowControlError {
                code: WorkflowControlErrorCode::Unavailable,
                message: "persistence unavailable".into(),
                retryable: true,
            },
        );

        let value = serde_json::to_value(response).unwrap();
        assert_eq!(value["result"]["status"], "error");
        assert_eq!(value["result"]["error"]["code"], "unavailable");
        assert_eq!(value["result"]["error"]["retryable"], true);
    }
}
