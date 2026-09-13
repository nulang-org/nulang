//! Versioned wire contract for native client actions.
//!
//! This module defines only the host/runtime protocol. It deliberately does
//! not make arbitrary exported functions callable from native UI. A later
//! compiler slice freezes authorized action entry points into `.nbc` metadata;
//! the runtime must consult that allowlist before executing a request.

use std::collections::BTreeMap;
use std::fmt;

use serde::{Deserialize, Serialize};

pub const CLIENT_ACTION_REQUEST_PROTOCOL: &str = "nulang-action-invoke/1";
pub const CLIENT_ACTION_RESULT_PROTOCOL: &str = "nulang-action-result/1";
pub const CLIENT_ACTION_MESSAGE_PROTOCOL: &str = "nulang-ui-msg/1";

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ClientActionRequest {
    #[serde(rename = "protocol")]
    pub protocol_version: String,
    pub handler: String,
    pub correlation_id: String,
    #[serde(default)]
    pub idempotency_key: String,
    #[serde(default)]
    pub form: BTreeMap<String, String>,
    #[serde(default)]
    pub signals: BTreeMap<String, String>,
}

impl ClientActionRequest {
    pub fn new(
        handler: impl Into<String>,
        correlation_id: impl Into<String>,
        idempotency_key: impl Into<String>,
    ) -> Self {
        Self {
            protocol_version: CLIENT_ACTION_REQUEST_PROTOCOL.to_string(),
            handler: handler.into(),
            correlation_id: correlation_id.into(),
            idempotency_key: idempotency_key.into(),
            form: BTreeMap::new(),
            signals: BTreeMap::new(),
        }
    }

    pub fn validate(&self) -> Result<(), ClientActionProtocolError> {
        if self.protocol_version != CLIENT_ACTION_REQUEST_PROTOCOL {
            return Err(ClientActionProtocolError::UnsupportedRequestProtocol {
                found: self.protocol_version.clone(),
            });
        }
        require_nonempty("handler", &self.handler)?;
        require_nonempty("correlation_id", &self.correlation_id)?;
        Ok(())
    }

    pub fn from_json(json: &str) -> Result<Self, ClientActionProtocolError> {
        let request: Self = serde_json::from_str(json)
            .map_err(|error| ClientActionProtocolError::InvalidRequestJson(error.to_string()))?;
        request.validate()?;
        Ok(request)
    }

    pub fn to_json(&self) -> Result<String, ClientActionProtocolError> {
        self.validate()?;
        serde_json::to_string(self)
            .map_err(|error| ClientActionProtocolError::InvalidRequestJson(error.to_string()))
    }
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct NativeUiMessageEnvelope {
    #[serde(rename = "protocol")]
    pub protocol_version: String,
    pub message: serde_json::Value,
}

impl NativeUiMessageEnvelope {
    pub fn validate(&self, index: usize) -> Result<(), ClientActionProtocolError> {
        if self.protocol_version != CLIENT_ACTION_MESSAGE_PROTOCOL {
            return Err(ClientActionProtocolError::InvalidMessageEnvelope {
                index,
                reason: format!(
                    "expected protocol '{}', got '{}'",
                    CLIENT_ACTION_MESSAGE_PROTOCOL, self.protocol_version
                ),
            });
        }
        if !self.message.is_object() {
            return Err(ClientActionProtocolError::InvalidMessageEnvelope {
                index,
                reason: "message must be a JSON object".to_string(),
            });
        }
        Ok(())
    }
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct ClientActionResult {
    #[serde(rename = "protocol")]
    pub protocol_version: String,
    pub correlation_id: String,
    #[serde(default)]
    pub messages: Vec<NativeUiMessageEnvelope>,
}

impl ClientActionResult {
    pub fn validate(
        &self,
        expected_correlation_id: &str,
    ) -> Result<(), ClientActionProtocolError> {
        if self.protocol_version != CLIENT_ACTION_RESULT_PROTOCOL {
            return Err(ClientActionProtocolError::UnsupportedResultProtocol {
                found: self.protocol_version.clone(),
            });
        }
        require_nonempty("correlation_id", &self.correlation_id)?;
        if self.correlation_id != expected_correlation_id {
            return Err(ClientActionProtocolError::CorrelationMismatch {
                expected: expected_correlation_id.to_string(),
                found: self.correlation_id.clone(),
            });
        }
        for (index, message) in self.messages.iter().enumerate() {
            message.validate(index)?;
        }
        Ok(())
    }

    pub fn from_json(
        json: &str,
        expected_correlation_id: &str,
    ) -> Result<Self, ClientActionProtocolError> {
        let result: Self = serde_json::from_str(json)
            .map_err(|error| ClientActionProtocolError::InvalidResultJson(error.to_string()))?;
        result.validate(expected_correlation_id)?;
        Ok(result)
    }

    pub fn to_json(
        &self,
        expected_correlation_id: &str,
    ) -> Result<String, ClientActionProtocolError> {
        self.validate(expected_correlation_id)?;
        serde_json::to_string(self)
            .map_err(|error| ClientActionProtocolError::InvalidResultJson(error.to_string()))
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ClientActionProtocolError {
    InvalidRequestJson(String),
    InvalidResultJson(String),
    UnsupportedRequestProtocol { found: String },
    UnsupportedResultProtocol { found: String },
    MissingField { field: &'static str },
    CorrelationMismatch { expected: String, found: String },
    InvalidMessageEnvelope { index: usize, reason: String },
}

impl fmt::Display for ClientActionProtocolError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::InvalidRequestJson(error) => {
                write!(f, "invalid client-action request JSON: {error}")
            }
            Self::InvalidResultJson(error) => {
                write!(f, "invalid client-action result JSON: {error}")
            }
            Self::UnsupportedRequestProtocol { found } => write!(
                f,
                "unsupported client-action request protocol '{found}'; expected '{}'",
                CLIENT_ACTION_REQUEST_PROTOCOL
            ),
            Self::UnsupportedResultProtocol { found } => write!(
                f,
                "unsupported client-action result protocol '{found}'; expected '{}'",
                CLIENT_ACTION_RESULT_PROTOCOL
            ),
            Self::MissingField { field } => {
                write!(f, "client-action field '{field}' must not be empty")
            }
            Self::CorrelationMismatch { expected, found } => write!(
                f,
                "client-action correlation mismatch: expected '{expected}', got '{found}'"
            ),
            Self::InvalidMessageEnvelope { index, reason } => {
                write!(f, "invalid client-action message {index}: {reason}")
            }
        }
    }
}

impl std::error::Error for ClientActionProtocolError {}

fn require_nonempty(
    field: &'static str,
    value: &str,
) -> Result<(), ClientActionProtocolError> {
    if value.trim().is_empty() {
        Err(ClientActionProtocolError::MissingField { field })
    } else {
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn request_round_trips_explicit_host_state() {
        let mut request = ClientActionRequest::new("save", "corr-1", "save:42:v1");
        request
            .form
            .insert("email".to_string(), "ada@example.com".to_string());
        request.signals.insert("count".to_string(), "3".to_string());

        let json = request.to_json().expect("encode request");
        let decoded = ClientActionRequest::from_json(&json).expect("decode request");
        assert_eq!(decoded, request);
        assert!(json.contains("nulang-action-invoke/1"));
    }

    #[test]
    fn request_rejects_wrong_major_and_missing_identity() {
        let wrong = r#"{"protocol":"nulang-action-invoke/2","handler":"save","correlation_id":"c"}"#;
        assert!(matches!(
            ClientActionRequest::from_json(wrong),
            Err(ClientActionProtocolError::UnsupportedRequestProtocol { .. })
        ));

        let missing = r#"{"protocol":"nulang-action-invoke/1","handler":"","correlation_id":"c"}"#;
        assert_eq!(
            ClientActionRequest::from_json(missing),
            Err(ClientActionProtocolError::MissingField { field: "handler" })
        );
    }

    #[test]
    fn result_validates_correlation_and_existing_ui_envelopes() {
        let result = ClientActionResult {
            protocol_version: CLIENT_ACTION_RESULT_PROTOCOL.to_string(),
            correlation_id: "corr-1".to_string(),
            messages: vec![NativeUiMessageEnvelope {
                protocol_version: CLIENT_ACTION_MESSAGE_PROTOCOL.to_string(),
                message: serde_json::json!({
                    "type": "signal.set",
                    "name": "count",
                    "value": "4"
                }),
            }],
        };

        let json = result.to_json("corr-1").expect("encode result");
        let decoded = ClientActionResult::from_json(&json, "corr-1").expect("decode result");
        assert_eq!(decoded, result);

        assert!(matches!(
            result.validate("different"),
            Err(ClientActionProtocolError::CorrelationMismatch { .. })
        ));
    }

    #[test]
    fn result_rejects_non_message_envelopes() {
        let result = ClientActionResult {
            protocol_version: CLIENT_ACTION_RESULT_PROTOCOL.to_string(),
            correlation_id: "corr-1".to_string(),
            messages: vec![NativeUiMessageEnvelope {
                protocol_version: CLIENT_ACTION_MESSAGE_PROTOCOL.to_string(),
                message: serde_json::Value::String("not-an-object".to_string()),
            }],
        };

        assert!(matches!(
            result.validate("corr-1"),
            Err(ClientActionProtocolError::InvalidMessageEnvelope { index: 0, .. })
        ));
    }
}
