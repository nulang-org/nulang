//! Host entry points must not mistake syntactically valid JSON for an
//! authorized, structurally valid Nulang UI message. Shared by web and native
//! consumers of nulang-ui-msg/1.
use nulang_ui_protocol::{
    decode_host_message, decode_runtime_message, HostToRuntimeMessage, Revision,
    RuntimeToHostMessage,
};
use serde_json::{json, Value};

fn action_message() -> Value {
    json!({
        "type": "invoke_action",
        "protocol": "nulang-ui-msg/1",
        "request": {
            "document_id": "tasks",
            "revision": "18446744073709551615",
            "action_id": "task.complete",
            "placement": "client",
            "correlation_id": "interaction-1",
            "idempotency_key": "mutation-1",
            "payload": { "type": "i64", "value": "-9223372036854775808" }
        }
    })
}

fn snapshot_message() -> Value {
    json!({
        "type": "snapshot",
        "protocol": "nulang-ui-msg/1",
        "document": {
            "protocol": "nulang-ui/1",
            "document_id": "tasks",
            "revision": "1",
            "root": "root",
            "nodes": [
                { "id": "root", "kind": "column", "children": ["task-1"] },
                { "id": "task-1", "kind": "text" }
            ]
        }
    })
}

#[test]
fn host_decoder_rejects_unsupported_envelope_version() {
    let mut message = action_message();
    message["protocol"] = json!("nulang-ui-msg/999");
    assert!(decode_host_message(&message.to_string()).is_err());
}

#[test]
fn host_decoder_rejects_blank_action_identity() {
    let mut message = action_message();
    message["request"]["action_id"] = json!(" ");
    assert!(decode_host_message(&message.to_string()).is_err());
}

#[test]
fn host_decoder_preserves_authorized_shape_and_lossless_payload() {
    let message = action_message();
    let decoded = decode_host_message(&message.to_string()).unwrap();
    decoded.validate().unwrap();
    let HostToRuntimeMessage::InvokeAction { request, .. } = decoded;
    assert_eq!(request.revision, Revision(u64::MAX));
    assert_eq!(
        serde_json::to_value(request.payload).unwrap(),
        json!({ "type": "i64", "value": "-9223372036854775808" })
    );
}

#[test]
fn runtime_decoder_rejects_unreachable_snapshot_nodes() {
    let mut message = snapshot_message();
    message["document"]["nodes"]
        .as_array_mut()
        .unwrap()
        .push(json!({ "id": "unreachable", "kind": "text" }));
    assert!(decode_runtime_message(&message.to_string()).is_err());
}

#[test]
fn runtime_decoder_rejects_non_increasing_patch_revision() {
    let message = json!({
        "type": "patch",
        "protocol": "nulang-ui-msg/1",
        "patch": {
            "protocol": "nulang-ui/1",
            "document_id": "tasks",
            "base_revision": "9",
            "revision": "9",
            "operations": []
        }
    });
    assert!(decode_runtime_message(&message.to_string()).is_err());
}

#[test]
fn runtime_decoder_rejects_ambiguous_per_node_event_bindings() {
    let mut message = snapshot_message();
    message["document"]["nodes"][0]["actions"] = json!([
        { "event": "press", "action_id": "task.one", "placement": "client" },
        { "event": "press", "action_id": "task.two", "placement": "client" }
    ]);
    assert!(decode_runtime_message(&message.to_string()).is_err());
}

#[test]
fn runtime_decoder_accepts_a_valid_snapshot() {
    let message = snapshot_message();
    let decoded = decode_runtime_message(&message.to_string()).unwrap();
    let RuntimeToHostMessage::Snapshot { document, .. } = decoded else {
        panic!("expected a snapshot");
    };
    assert_eq!(document.revision, Revision(1));
    assert_eq!(document.nodes.len(), 2);
}
