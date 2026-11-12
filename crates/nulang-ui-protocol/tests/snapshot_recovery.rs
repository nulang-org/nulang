use nulang_ui_protocol::{
    NodeId, ProtocolError, Revision, UiDocument, UiHostState, UiNode, UiPatch, WireValue,
};

fn document(revision: u64, value: &str) -> UiDocument {
    let mut root = UiNode::new("root", "form");
    root.children.push(NodeId::from("title"));
    let mut input = UiNode::new("title", "input");
    input
        .properties
        .insert("value".to_owned(), WireValue::from(value));
    UiDocument::new("session-1", Revision(revision), "root", vec![root, input])
}

#[test]
fn snapshot_requires_explicit_discard_when_input_is_pending() {
    let mut host = UiHostState::new(document(1, "")).unwrap();
    host.record_input(
        &NodeId::from("title"),
        "value",
        Revision(7),
        WireValue::from("local"),
    )
    .unwrap();
    let before = host.document().clone();

    assert!(matches!(
        host.resync(document(4, "remote"), false),
        Err(ProtocolError::SnapshotWouldDiscardPending)
    ));
    assert_eq!(host.document(), &before);
    host.resync(document(4, "remote"), true).unwrap();
    let input = host
        .document()
        .nodes
        .iter()
        .find(|n| n.id.as_str() == "title")
        .unwrap();
    assert_eq!(input.properties["value"], WireValue::from("remote"));
    // A reconnect cannot reuse a sequence that may still be in flight.
    assert!(matches!(
        host.record_input(
            &NodeId::from("title"),
            "value",
            Revision(7),
            WireValue::from("replay")
        ),
        Err(ProtocolError::NonIncreasingInputSequence { .. })
    ));
    host.record_input(
        &NodeId::from("title"),
        "value",
        Revision(8),
        WireValue::from("fresh"),
    )
    .unwrap();
}

#[test]
fn snapshot_rejects_rollbacks_cross_document_and_invalid_trees() {
    let mut host = UiHostState::new(document(4, "server")).unwrap();
    assert!(matches!(
        host.resync(document(3, "old"), true),
        Err(ProtocolError::StaleSnapshot { .. })
    ));
    let mut other = document(6, "cross-scope");
    other.document_id = "other".into();
    assert!(matches!(
        host.resync(other, true),
        Err(ProtocolError::DocumentIdMismatch { .. })
    ));
    let mut invalid = document(6, "bad");
    invalid.nodes.retain(|n| n.id.as_str() != "title");
    assert!(host.resync(invalid, true).is_err());
    assert_eq!(host.document(), &document(4, "server"));
}

#[test]
fn snapshot_repairs_patch_revision_gap_without_replaying_events() {
    let mut host = UiHostState::new(document(1, "one")).unwrap();
    let gap = UiPatch::new("session-1", Revision(3), Revision(4), vec![]);
    assert!(host.apply_server_patch(&gap).is_err());
    host.resync(document(3, "three"), false).unwrap();
    assert!(host.apply_server_patch(&gap).is_ok());
    assert_eq!(host.document().revision, Revision(4));
}
