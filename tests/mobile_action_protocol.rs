use nulang::mobile::action::{
    decode_client_action_invocation, decode_client_action_output,
    encode_client_action_invocation, encode_client_action_output, stable_client_action_id,
    ClientActionProtocolError,
};
use nulang_ui_protocol::{
    ActionId, ActionPlacement, ActionRequest, CorrelationId, DocumentId, IdempotencyKey,
    Revision, RuntimeToHostMessage, UiPatch, WireValue,
};

fn request(placement: ActionPlacement) -> ActionRequest {
    ActionRequest {
        document_id: DocumentId::new("doc-public"),
        revision: Revision(41),
        action_id: ActionId::new("action-public"),
        placement,
        correlation_id: CorrelationId::new("corr-public"),
        idempotency_key: IdempotencyKey::new("idem-public"),
        payload: WireValue::from("payload"),
    }
}

#[test]
fn public_native_action_transport_reuses_frozen_host_message() {
    let request = request(ActionPlacement::Client);
    let json = encode_client_action_invocation(&request).expect("encode action invocation");
    let decoded = decode_client_action_invocation(&json).expect("decode action invocation");

    assert_eq!(decoded, request);
    assert!(json.contains("nulang-ui-msg/1"));
    assert!(json.contains("invoke_action"));
    assert!(!json.contains("nulang-action-invoke"));
}

#[test]
fn public_native_action_transport_rejects_server_placement() {
    assert_eq!(
        encode_client_action_invocation(&request(ActionPlacement::Server)),
        Err(ClientActionProtocolError::ServerPlacement)
    );
}

#[test]
fn public_native_action_output_is_a_canonical_runtime_message() {
    let message = RuntimeToHostMessage::patch(UiPatch::new(
        DocumentId::new("doc-public"),
        Revision(41),
        Revision(42),
        Vec::new(),
    ));
    let json = encode_client_action_output(&message).expect("encode runtime message");
    let decoded = decode_client_action_output(&json).expect("decode runtime message");

    assert_eq!(decoded, message);
    assert!(json.contains("nulang-ui-msg/1"));
    assert!(json.contains("patch"));
    assert!(!json.contains("nulang-action-result"));
}

#[test]
fn public_action_identity_is_deterministic_without_exposing_handler_name() {
    let action = stable_client_action_id("field-app", "task_complete").expect("action id");
    let again = stable_client_action_id("field-app", "task_complete").expect("action id");
    let different = stable_client_action_id("field-app", "task_delete").expect("action id");

    assert_eq!(action, again);
    assert_ne!(action, different);
    assert!(action.as_str().starts_with("action_"));
    assert!(!action.as_str().contains("task_complete"));
}
