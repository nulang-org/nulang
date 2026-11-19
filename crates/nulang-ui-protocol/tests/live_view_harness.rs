use nulang_ui_protocol::testing::{UiTestError, UiTestHarness};
use nulang_ui_protocol::{
    ActionBinding, ActionId, ActionPlacement, DocumentId, HostToRuntimeMessage, NodeId,
    PatchOperation, ProtocolError, Revision, RuntimeToHostMessage, UiDocument, UiNode, UiPatch,
    WireValue,
};

fn snapshot() -> RuntimeToHostMessage {
    let mut root = UiNode::new("root", "column");
    root.children = vec![NodeId::from("save"), NodeId::from("cancel")];

    let mut save = UiNode::new("save", "button");
    save.properties
        .insert("label".into(), WireValue::from("Save"));
    save.actions.push(ActionBinding {
        event: "click".into(),
        action_id: ActionId::from("save_record"),
        placement: ActionPlacement::Server,
    });
    let mut cancel = UiNode::new("cancel", "button");
    cancel.actions.push(ActionBinding {
        event: "click".into(),
        action_id: ActionId::from("close_dialog"),
        placement: ActionPlacement::Client,
    });
    RuntimeToHostMessage::snapshot(UiDocument::new(
        "editor",
        Revision(1),
        "root",
        vec![root, save, cancel],
    ))
}

fn label_patch(base: u64, next: u64, value: &str) -> RuntimeToHostMessage {
    RuntimeToHostMessage::patch(UiPatch::new(
        "editor",
        Revision(base),
        Revision(next),
        vec![PatchOperation::SetProperty {
            node_id: NodeId::from("save"),
            name: "label".into(),
            value: WireValue::from(value),
        }],
    ))
}

#[test]
fn mount_and_apply_wire_messages_without_a_browser() {
    let mut host = UiTestHarness::mount(snapshot()).unwrap();
    assert_eq!(host.document().revision, Revision(1));
    assert_eq!(
        host.node("save").unwrap().properties["label"],
        WireValue::from("Save")
    );

    host.receive(label_patch(1, 2, "Saved")).unwrap();
    assert_eq!(host.document().revision, Revision(2));
    assert_eq!(
        host.node("save").unwrap().properties["label"],
        WireValue::from("Saved")
    );
}

#[test]
fn invalid_and_stale_patches_do_not_mutate_test_state() {
    let mut host = UiTestHarness::mount(snapshot()).unwrap();
    let invalid = RuntimeToHostMessage::patch(UiPatch::new(
        "editor",
        Revision(1),
        Revision(2),
        vec![
            PatchOperation::SetProperty {
                node_id: NodeId::from("save"),
                name: "label".into(),
                value: WireValue::from("Changed"),
            },
            PatchOperation::RemoveNode {
                node_id: NodeId::from("missing"),
            },
        ],
    ));
    let before = host.document().clone();
    assert!(matches!(
        host.receive(invalid),
        Err(UiTestError::Protocol(ProtocolError::MissingNode(_)))
    ));
    assert_eq!(host.document(), &before);
    assert!(matches!(
        host.receive(label_patch(2, 3, "wrong base")),
        Err(UiTestError::Protocol(
            ProtocolError::RevisionMismatch { .. }
        ))
    ));
    assert_eq!(host.document(), &before);
}

#[test]
fn reconnect_snapshots_never_roll_back_or_conflict_with_current_revision() {
    let mut host = UiTestHarness::mount(snapshot()).unwrap();
    host.receive(label_patch(1, 2, "Saved")).unwrap();
    let before = host.document().clone();

    assert!(matches!(
        host.receive(snapshot()),
        Err(UiTestError::StaleSnapshot { .. })
    ));
    let mut conflict = before.clone();
    conflict
        .nodes
        .iter_mut()
        .find(|n| n.id.as_str() == "save")
        .unwrap()
        .properties
        .insert("label".into(), WireValue::from("Different"));
    assert!(matches!(
        host.receive(RuntimeToHostMessage::snapshot(conflict)),
        Err(UiTestError::ConflictingSnapshot { .. })
    ));
    host.receive(RuntimeToHostMessage::snapshot(before.clone()))
        .unwrap();
    let mut foreign = before.clone();
    foreign.document_id = DocumentId::from("another");
    assert!(matches!(
        host.receive(RuntimeToHostMessage::snapshot(foreign)),
        Err(UiTestError::Protocol(
            ProtocolError::DocumentIdMismatch { .. }
        ))
    ));
    assert_eq!(host.document(), &before);
}

#[test]
fn actions_are_checked_against_the_current_component_event_and_revision() {
    let mut host = UiTestHarness::mount(snapshot()).unwrap();
    host.receive(label_patch(1, 2, "Saved")).unwrap();

    let message = host
        .action(
            "save",
            "click",
            WireValue::from("record-9"),
            "corr-9",
            "idem-9",
        )
        .unwrap();
    message.validate().unwrap();
    match message {
        HostToRuntimeMessage::InvokeAction { request, .. } => {
            assert_eq!(request.document_id, DocumentId::from("editor"));
            assert_eq!(request.revision, Revision(2));
            assert_eq!(request.action_id, ActionId::from("save_record"));
            assert_eq!(request.placement, ActionPlacement::Server);
            assert_eq!(request.payload, WireValue::from("record-9"));
            assert_eq!(request.correlation_id.as_str(), "corr-9");
            assert_eq!(request.idempotency_key.as_str(), "idem-9");
        }
    }

    assert!(matches!(
        host.action("absent", "click", WireValue::Null, "c", "i"),
        Err(UiTestError::UnknownNode(_))
    ));
    assert!(matches!(
        host.action("save", "input", WireValue::Null, "c", "i"),
        Err(UiTestError::UnknownAction { .. })
    ));
    assert!(matches!(
        host.action("cancel", "click", WireValue::Null, "c", "i"),
        Err(UiTestError::ClientAction { .. })
    ));
    assert!(matches!(
        host.action("save", "click", WireValue::Null, "", "i"),
        Err(UiTestError::Protocol(ProtocolError::EmptyIdentifier(_)))
    ));
}

#[test]
fn only_valid_initial_snapshots_can_mount() {
    assert!(matches!(
        UiTestHarness::mount(label_patch(1, 2, "bad")),
        Err(UiTestError::ExpectedSnapshot)
    ));
    let mut invalid = match snapshot() {
        RuntimeToHostMessage::Snapshot { document, .. } => document,
        _ => unreachable!(),
    };
    invalid.root = NodeId::from("missing");
    assert!(matches!(
        UiTestHarness::mount(RuntimeToHostMessage::snapshot(invalid)),
        Err(UiTestError::Protocol(ProtocolError::MissingNode(_)))
    ));
}

#[test]
fn ambiguous_component_event_bindings_are_not_dispatched() {
    let mut doc = match snapshot() {
        RuntimeToHostMessage::Snapshot { document, .. } => document,
        _ => unreachable!(),
    };
    let save = doc
        .nodes
        .iter_mut()
        .find(|n| n.id.as_str() == "save")
        .unwrap();
    save.actions.push(ActionBinding {
        event: "click".into(),
        action_id: ActionId::from("other_save"),
        placement: ActionPlacement::Server,
    });
    let host = UiTestHarness::mount(RuntimeToHostMessage::snapshot(doc)).unwrap();
    assert!(matches!(
        host.action("save", "click", WireValue::Null, "corr", "idem"),
        Err(UiTestError::AmbiguousAction { .. })
    ));
}

#[test]
fn valid_newer_snapshot_recovers_but_bad_message_version_does_not() {
    let mut host = UiTestHarness::mount(snapshot()).unwrap();
    let mut fresh = host.document().clone();
    fresh.revision = Revision(7);
    fresh
        .nodes
        .iter_mut()
        .find(|n| n.id.as_str() == "save")
        .unwrap()
        .properties
        .insert("label".into(), WireValue::from("Recovered"));
    host.receive(RuntimeToHostMessage::snapshot(fresh.clone()))
        .unwrap();

    let mut invalid = RuntimeToHostMessage::snapshot(fresh.clone());
    if let RuntimeToHostMessage::Snapshot { protocol, .. } = &mut invalid {
        *protocol = "nulang-ui-msg/unknown".into();
    }
    assert!(matches!(
        host.receive(invalid),
        Err(UiTestError::Protocol(
            ProtocolError::UnsupportedProtocol { .. }
        ))
    ));
    assert_eq!(host.document(), &fresh);
}
