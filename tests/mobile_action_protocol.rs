use std::collections::BTreeMap;

use nulang::mobile::action::{
    ClientActionProtocolError, ClientActionRequest, ClientActionResult,
    NativeUiMessageEnvelope, CLIENT_ACTION_MESSAGE_PROTOCOL,
    CLIENT_ACTION_REQUEST_PROTOCOL, CLIENT_ACTION_RESULT_PROTOCOL,
};

#[test]
fn public_action_request_round_trips_explicit_snapshot_state() {
    let mut request = ClientActionRequest::new("task.complete", "corr-42", "task:42:v7");
    request.form = BTreeMap::from([("note".to_string(), "done".to_string())]);
    request.signals = BTreeMap::from([("count".to_string(), "3".to_string())]);

    let json = request.to_json().expect("encode client action request");
    let decoded = ClientActionRequest::from_json(&json).expect("decode client action request");

    assert_eq!(decoded, request);
    assert_eq!(decoded.protocol_version, CLIENT_ACTION_REQUEST_PROTOCOL);
}

#[test]
fn public_action_result_reuses_versioned_ui_message_envelopes() {
    let result = ClientActionResult {
        protocol_version: CLIENT_ACTION_RESULT_PROTOCOL.to_string(),
        correlation_id: "corr-42".to_string(),
        messages: vec![NativeUiMessageEnvelope {
            protocol_version: CLIENT_ACTION_MESSAGE_PROTOCOL.to_string(),
            message: serde_json::json!({
                "type": "signal.set",
                "name": "count",
                "value": "4"
            }),
        }],
    };

    let json = result.to_json("corr-42").expect("encode action result");
    let decoded =
        ClientActionResult::from_json(&json, "corr-42").expect("decode action result");
    assert_eq!(decoded, result);
}

#[test]
fn action_result_cannot_switch_correlations() {
    let result = ClientActionResult {
        protocol_version: CLIENT_ACTION_RESULT_PROTOCOL.to_string(),
        correlation_id: "wrong".to_string(),
        messages: vec![],
    };

    assert!(matches!(
        result.validate("expected"),
        Err(ClientActionProtocolError::CorrelationMismatch { .. })
    ));
}
