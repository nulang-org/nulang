//! Versioned transport-neutral workflow runtime control protocol.
//!
//! This crate owns only the control-plane vocabulary shared by Nulang runtimes
//! and hosts. It deliberately contains no runtime implementation, storage
//! backend, transport client, Cloud API model, or workflow-definition graph.

use serde::{Deserialize, Serialize};
use serde_json::Value;
use std::collections::BTreeMap;
use std::fmt;

pub const WORKFLOW_CONTROL_PROTOCOL_VERSION: &str = "nulang-workflow-control/v0alpha1";

macro_rules! string_id {
    ($name:ident) => {
        #[derive(Debug, Clone, PartialEq, Eq, PartialOrd, Ord, Hash, Serialize, Deserialize)]
        #[serde(transparent)]
        pub struct $name(pub String);

        impl $name {
            pub fn new(value: impl Into<String>) -> Self {
                Self(value.into())
            }

            pub fn as_str(&self) -> &str {
                &self.0
            }
        }

        impl From<&str> for $name {
            fn from(value: &str) -> Self {
                Self(value.to_owned())
            }
        }

        impl From<String> for $name {
            fn from(value: String) -> Self {
                Self(value)
            }
        }

        impl fmt::Display for $name {
            fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
                f.write_str(&self.0)
            }
        }
    };
}

string_id!(WorkflowDefinitionId);
string_id!(WorkflowInstanceId);
string_id!(WorkflowRequestId);

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum WorkflowLifecycleStatus {
    Pending,
    Running,
    Waiting,
    Completed,
    Failed,
    Cancelled,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "kind", rename_all = "snake_case")]
pub enum WorkflowWait {
    Signal { name: String },
    Timer { name: String },
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct WorkflowFailure {
    pub code: String,
    pub message: String,
    #[serde(default)]
    pub retryable: bool,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct WorkflowSnapshot {
    pub instance_id: WorkflowInstanceId,
    pub definition_id: WorkflowDefinitionId,
    pub status: WorkflowLifecycleStatus,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub current_step: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub waiting_on: Option<WorkflowWait>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub output: Option<Value>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub failure: Option<WorkflowFailure>,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(tag = "operation", rename_all = "snake_case")]
pub enum WorkflowControlCommand {
    Start {
        definition_id: WorkflowDefinitionId,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        instance_id: Option<WorkflowInstanceId>,
        #[serde(default)]
        input: BTreeMap<String, Value>,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        idempotency_key: Option<String>,
    },
    Inspect {
        instance_id: WorkflowInstanceId,
    },
    Signal {
        instance_id: WorkflowInstanceId,
        signal: String,
        #[serde(default)]
        payload: Value,
    },
    Cancel {
        instance_id: WorkflowInstanceId,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        reason: Option<String>,
    },
    Query {
        instance_id: WorkflowInstanceId,
        query: String,
        #[serde(default)]
        args: Vec<Value>,
    },
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct WorkflowControlRequest {
    pub protocol: String,
    pub request_id: WorkflowRequestId,
    pub command: WorkflowControlCommand,
}

impl WorkflowControlRequest {
    pub fn new(
        request_id: impl Into<WorkflowRequestId>,
        command: WorkflowControlCommand,
    ) -> Self {
        Self {
            protocol: WORKFLOW_CONTROL_PROTOCOL_VERSION.to_owned(),
            request_id: request_id.into(),
            command,
        }
    }

    pub fn validate(&self) -> Result<(), WorkflowProtocolError> {
        validate_protocol_version(&self.protocol)
    }
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(tag = "kind", rename_all = "snake_case")]
pub enum WorkflowControlReply {
    Started { workflow: WorkflowSnapshot },
    Inspected { workflow: WorkflowSnapshot },
    Signaled { workflow: WorkflowSnapshot },
    Cancelled { workflow: WorkflowSnapshot },
    QueryResult { value: Value },
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum WorkflowControlErrorCode {
    NotFound,
    InvalidRequest,
    Conflict,
    Unavailable,
    Unsupported,
    Internal,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct WorkflowControlError {
    pub code: WorkflowControlErrorCode,
    pub message: String,
    #[serde(default)]
    pub retryable: bool,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(tag = "status", rename_all = "snake_case")]
pub enum WorkflowControlResult {
    Ok { reply: WorkflowControlReply },
    Error { error: WorkflowControlError },
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct WorkflowControlResponse {
    pub protocol: String,
    pub request_id: WorkflowRequestId,
    pub result: WorkflowControlResult,
}

impl WorkflowControlResponse {
    pub fn ok(
        request_id: impl Into<WorkflowRequestId>,
        reply: WorkflowControlReply,
    ) -> Self {
        Self {
            protocol: WORKFLOW_CONTROL_PROTOCOL_VERSION.to_owned(),
            request_id: request_id.into(),
            result: WorkflowControlResult::Ok { reply },
        }
    }

    pub fn error(
        request_id: impl Into<WorkflowRequestId>,
        error: WorkflowControlError,
    ) -> Self {
        Self {
            protocol: WORKFLOW_CONTROL_PROTOCOL_VERSION.to_owned(),
            request_id: request_id.into(),
            result: WorkflowControlResult::Error { error },
        }
    }

    pub fn validate(&self) -> Result<(), WorkflowProtocolError> {
        validate_protocol_version(&self.protocol)
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum WorkflowProtocolError {
    UnsupportedVersion(String),
}

impl fmt::Display for WorkflowProtocolError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::UnsupportedVersion(version) => {
                write!(f, "unsupported workflow control protocol: {version}")
            }
        }
    }
}

impl std::error::Error for WorkflowProtocolError {}

pub fn validate_protocol_version(protocol: &str) -> Result<(), WorkflowProtocolError> {
    if protocol == WORKFLOW_CONTROL_PROTOCOL_VERSION {
        Ok(())
    } else {
        Err(WorkflowProtocolError::UnsupportedVersion(
            protocol.to_owned(),
        ))
    }
}

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
