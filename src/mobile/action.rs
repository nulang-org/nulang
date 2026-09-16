//! Canonical native client-action transport helpers.
//!
//! Native hosts do not get a second action protocol. They send the frozen
//! `nulang-ui-msg/1` `HostToRuntimeMessage::InvokeAction` envelope and receive
//! a frozen `RuntimeToHostMessage` (`snapshot` or `patch`) in return.

use std::fmt;

use nulang_ui_protocol::{
    decode_host_message, decode_runtime_message, encode_host_message, encode_runtime_message,
    ActionId, ActionPlacement, ActionRequest, HostToRuntimeMessage, RuntimeToHostMessage,
};

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ClientActionProtocolError {
    InvalidHostJson(String),
    InvalidRuntimeJson(String),
    InvalidHostMessage(String),
    InvalidRuntimeMessage(String),
    ServerPlacement,
    EmptyIdentity(&'static str),
}

impl fmt::Display for ClientActionProtocolError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::InvalidHostJson(error) => {
                write!(f, "invalid host action JSON: {error}")
            }
            Self::InvalidRuntimeJson(error) => {
                write!(f, "invalid runtime action JSON: {error}")
            }
            Self::InvalidHostMessage(error) => {
                write!(f, "invalid host action message: {error}")
            }
            Self::InvalidRuntimeMessage(error) => {
                write!(f, "invalid runtime action message: {error}")
            }
            Self::ServerPlacement => {
                write!(
                    f,
                    "server-placed actions cannot execute in the local native runtime"
                )
            }
            Self::EmptyIdentity(field) => write!(f, "{field} must not be empty"),
        }
    }
}

impl std::error::Error for ClientActionProtocolError {}

/// Decode and validate one canonical native client-action invocation.
///
/// Server-placed actions fail closed here so local native runtimes cannot
/// accidentally bypass the explicit network/server policy layer.
pub fn decode_client_action_invocation(
    input: &str,
) -> Result<ActionRequest, ClientActionProtocolError> {
    let message = decode_host_message(input)
        .map_err(|error| ClientActionProtocolError::InvalidHostJson(error.to_string()))?;
    message
        .validate()
        .map_err(|error| ClientActionProtocolError::InvalidHostMessage(error.to_string()))?;

    match message {
        HostToRuntimeMessage::InvokeAction { request, .. } => {
            require_client_placement(&request)?;
            Ok(request)
        }
    }
}

/// Encode one canonical native client-action invocation.
pub fn encode_client_action_invocation(
    request: &ActionRequest,
) -> Result<String, ClientActionProtocolError> {
    request
        .validate()
        .map_err(|error| ClientActionProtocolError::InvalidHostMessage(error.to_string()))?;
    require_client_placement(request)?;
    encode_host_message(&HostToRuntimeMessage::invoke_action(request.clone()))
        .map_err(|error| ClientActionProtocolError::InvalidHostJson(error.to_string()))
}

/// Decode and validate the canonical runtime output produced by a client reducer.
pub fn decode_client_action_output(
    input: &str,
) -> Result<RuntimeToHostMessage, ClientActionProtocolError> {
    let message = decode_runtime_message(input)
        .map_err(|error| ClientActionProtocolError::InvalidRuntimeJson(error.to_string()))?;
    message
        .validate()
        .map_err(|error| ClientActionProtocolError::InvalidRuntimeMessage(error.to_string()))?;
    Ok(message)
}

/// Encode one canonical runtime output from a client reducer.
pub fn encode_client_action_output(
    message: &RuntimeToHostMessage,
) -> Result<String, ClientActionProtocolError> {
    message
        .validate()
        .map_err(|error| ClientActionProtocolError::InvalidRuntimeMessage(error.to_string()))?;
    encode_runtime_message(message)
        .map_err(|error| ClientActionProtocolError::InvalidRuntimeJson(error.to_string()))
}

/// Produce a deterministic opaque action identity for a compiled module/handler pair.
///
/// The handler name remains compiler/audit metadata. Native hosts see this ID,
/// so source-level function names do not become the authorization surface.
pub fn stable_client_action_id(
    module_name: &str,
    handler_name: &str,
) -> Result<ActionId, ClientActionProtocolError> {
    if module_name.trim().is_empty() {
        return Err(ClientActionProtocolError::EmptyIdentity("module_name"));
    }
    if handler_name.trim().is_empty() {
        return Err(ClientActionProtocolError::EmptyIdentity("handler_name"));
    }

    let mut hasher = blake3::Hasher::new();
    hasher.update(b"nulang-client-action-id/v1\0");
    hasher.update(module_name.as_bytes());
    hasher.update(&[0]);
    hasher.update(handler_name.as_bytes());
    Ok(ActionId::new(format!(
        "action_{}",
        hasher.finalize().to_hex()
    )))
}

fn require_client_placement(request: &ActionRequest) -> Result<(), ClientActionProtocolError> {
    if request.placement == ActionPlacement::Client {
        Ok(())
    } else {
        Err(ClientActionProtocolError::ServerPlacement)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use nulang_ui_protocol::{
        CorrelationId, DocumentId, IdempotencyKey, Revision, UiPatch, WireValue,
    };

    fn request(placement: ActionPlacement) -> ActionRequest {
        ActionRequest {
            document_id: DocumentId::new("doc-1"),
            revision: Revision(7),
            action_id: ActionId::new("action-1"),
            placement,
            correlation_id: CorrelationId::new("corr-1"),
            idempotency_key: IdempotencyKey::new("idem-1"),
            payload: WireValue::from("payload"),
        }
    }

    #[test]
    fn canonical_client_invocation_round_trips() {
        let request = request(ActionPlacement::Client);
        let json = encode_client_action_invocation(&request).expect("encode invocation");
        let decoded = decode_client_action_invocation(&json).expect("decode invocation");
        assert_eq!(decoded, request);
        assert!(json.contains("nulang-ui-msg/1"));
        assert!(json.contains("invoke_action"));
    }

    #[test]
    fn server_action_fails_closed_locally() {
        let error = encode_client_action_invocation(&request(ActionPlacement::Server))
            .expect_err("server action must not execute locally");
        assert_eq!(error, ClientActionProtocolError::ServerPlacement);
    }

    #[test]
    fn canonical_runtime_patch_round_trips() {
        let message = RuntimeToHostMessage::patch(UiPatch::new(
            DocumentId::new("doc-1"),
            Revision(7),
            Revision(8),
            Vec::new(),
        ));
        let json = encode_client_action_output(&message).expect("encode output");
        let decoded = decode_client_action_output(&json).expect("decode output");
        assert_eq!(decoded, message);
        assert!(json.contains("nulang-ui-msg/1"));
        assert!(json.contains("patch"));
    }

    #[test]
    fn stable_action_ids_are_opaque_and_deterministic() {
        let first = stable_client_action_id("app", "save").expect("action id");
        let second = stable_client_action_id("app", "save").expect("action id");
        let other = stable_client_action_id("app", "delete").expect("action id");
        assert_eq!(first, second);
        assert_ne!(first, other);
        assert_ne!(first.as_str(), "save");
        assert!(first.as_str().starts_with("action_"));
    }
}
