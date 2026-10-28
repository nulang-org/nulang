use nulang_ui_protocol::{
    InputAcknowledgement, NodeId, PatchApplyOutcome, PatchOperation, ProtocolError, Revision,
    UiDocument, UiHostState, UiNode, UiPatch, WireValue,
};

fn node(id: &str, kind: &str) -> UiNode {
    UiNode::new(id, kind)
}

fn list_document(revision: u64, items: &[&str]) -> UiDocument {
    let mut root = node("root", "list");
    root.children = items.iter().map(|id| NodeId::from(*id)).collect();
    let mut nodes = vec![root];
    nodes.extend(items.iter().map(|id| node(id, "row")));
    UiDocument::new("tasks", Revision(revision), "root", nodes)
}

fn input_document() -> UiDocument {
    let mut input = node("title", "input");
    input.properties.insert("value".into(), WireValue::from(""));
    let mut root = node("root", "form");
    root.children.push(NodeId::from("title"));
    UiDocument::new("form-1", Revision(1), "root", vec![root, input])
}

#[test]
fn keyed_reorder_uses_one_children_patch_and_retains_row_identity() {
    let original = list_document(1, &["a", "b", "c"]);
    let target = list_document(2, &["c", "a", "b"]);
    let patch = original.diff(&target).unwrap();
    assert_eq!(
        patch.operations,
        vec![PatchOperation::ReplaceChildren {
            node_id: NodeId::from("root"),
            children: vec![NodeId::from("c"), NodeId::from("a"), NodeId::from("b")],
        }]
    );
    let mut current = original;
    current.apply_patch(&patch).unwrap();
    assert_eq!(current, target);
}

#[test]
fn keyed_diff_changes_only_the_affected_property() {
    let mut original = list_document(1, &["a", "b"]);
    original
        .nodes
        .iter_mut()
        .find(|n| n.id.as_str() == "a")
        .unwrap()
        .properties
        .insert("title".into(), WireValue::from("old"));
    let mut target = original.clone();
    target.revision = Revision(2);
    target
        .nodes
        .iter_mut()
        .find(|n| n.id.as_str() == "a")
        .unwrap()
        .properties
        .insert("title".into(), WireValue::from("new"));
    let patch = original.diff(&target).unwrap();
    assert_eq!(
        patch.operations,
        vec![PatchOperation::SetProperty {
            node_id: NodeId::from("a"),
            name: "title".into(),
            value: WireValue::from("new")
        }]
    );
    original.apply_patch(&patch).unwrap();
    assert_eq!(original, target);
}

#[test]
fn keyed_insert_and_remove_round_trip_atomically() {
    let original = list_document(1, &["a", "b"]);
    let target = list_document(2, &["b", "c"]);
    let patch = original.diff(&target).unwrap();
    assert!(patch.operations.contains(&PatchOperation::UpsertNode {
        node: node("c", "row")
    }));
    assert!(patch.operations.contains(&PatchOperation::RemoveNode {
        node_id: NodeId::from("a")
    }));
    let mut actual = original;
    actual.apply_patch(&patch).unwrap();
    assert_eq!(actual, target);
}

#[test]
fn keyed_diff_refuses_wrong_document_or_non_increasing_revision() {
    let original = list_document(3, &["a"]);
    let mut target = list_document(2, &["a"]);
    assert!(matches!(
        original.diff(&target),
        Err(ProtocolError::NonIncreasingRevision { .. })
    ));
    target.revision = Revision(4);
    target.document_id = "other".into();
    assert!(matches!(
        original.diff(&target),
        Err(ProtocolError::DocumentIdMismatch { .. })
    ));
}

#[test]
fn pending_input_survives_stale_server_values_until_its_own_acknowledgement() {
    let mut host = UiHostState::new(input_document()).unwrap();
    let title = NodeId::from("title");
    host.record_input(&title, "value", Revision(1), WireValue::from("ab"))
        .unwrap();
    host.record_input(&title, "value", Revision(2), WireValue::from("abc"))
        .unwrap();

    let first = UiPatch::new(
        "form-1",
        Revision(1),
        Revision(2),
        vec![PatchOperation::SetProperty {
            node_id: title.clone(),
            name: "value".into(),
            value: WireValue::from("ab"),
        }],
    )
    .with_input_acknowledgements(vec![InputAcknowledgement {
        node_id: title.clone(),
        property: "value".into(),
        sequence: Revision(1),
    }]);
    assert_eq!(
        host.apply_server_patch(&first).unwrap(),
        PatchApplyOutcome::Applied
    );
    assert_eq!(
        host.document()
            .nodes
            .iter()
            .find(|n| n.id == title)
            .unwrap()
            .properties["value"],
        WireValue::from("abc")
    );

    let next = UiPatch::new(
        "form-1",
        Revision(2),
        Revision(3),
        vec![PatchOperation::SetProperty {
            node_id: title.clone(),
            name: "value".into(),
            value: WireValue::from("abc"),
        }],
    )
    .with_input_acknowledgements(vec![InputAcknowledgement {
        node_id: title.clone(),
        property: "value".into(),
        sequence: Revision(2),
    }]);
    host.apply_server_patch(&next).unwrap();
    let canonical = UiPatch::new(
        "form-1",
        Revision(3),
        Revision(4),
        vec![PatchOperation::SetProperty {
            node_id: title.clone(),
            name: "value".into(),
            value: WireValue::from("ABC"),
        }],
    );
    host.apply_server_patch(&canonical).unwrap();
    assert_eq!(
        host.document()
            .nodes
            .iter()
            .find(|n| n.id == title)
            .unwrap()
            .properties["value"],
        WireValue::from("ABC")
    );
}

#[test]
fn duplicate_patches_are_ignored_but_missing_revisions_require_resync() {
    let mut host = UiHostState::new(input_document()).unwrap();
    let patch = UiPatch::new("form-1", Revision(1), Revision(2), vec![]);
    assert_eq!(
        host.apply_server_patch(&patch).unwrap(),
        PatchApplyOutcome::Applied
    );
    assert_eq!(
        host.apply_server_patch(&patch).unwrap(),
        PatchApplyOutcome::IgnoredStale
    );
    assert_eq!(host.document().revision, Revision(2));
    let gap = UiPatch::new("form-1", Revision(3), Revision(4), vec![]);
    assert!(matches!(
        host.apply_server_patch(&gap),
        Err(ProtocolError::RevisionMismatch { .. })
    ));
    assert_eq!(host.document().revision, Revision(2));
}

#[test]
fn local_input_sequences_are_monotonic_and_only_input_properties_are_editable() {
    let mut host = UiHostState::new(input_document()).unwrap();
    let title = NodeId::from("title");
    host.record_input(&title, "value", Revision(3), WireValue::from("x"))
        .unwrap();
    assert!(matches!(
        host.record_input(&title, "value", Revision(3), WireValue::from("y")),
        Err(ProtocolError::NonIncreasingInputSequence { .. })
    ));
    assert!(matches!(
        host.record_input(&title, "class", Revision(4), WireValue::from("admin")),
        Err(ProtocolError::UnsupportedLocalInputProperty(_))
    ));
    assert!(matches!(
        host.record_input(
            &NodeId::from("missing"),
            "value",
            Revision(1),
            WireValue::from("x")
        ),
        Err(ProtocolError::MissingNode(_))
    ));
}

#[test]
fn malformed_acknowledgements_fail_without_mutating_the_host() {
    let mut host = UiHostState::new(input_document()).unwrap();
    let previous = host.document().clone();
    let patch = UiPatch::new("form-1", Revision(1), Revision(2), vec![])
        .with_input_acknowledgements(vec![
            InputAcknowledgement {
                node_id: NodeId::from("title"),
                property: "value".into(),
                sequence: Revision(1),
            },
            InputAcknowledgement {
                node_id: NodeId::from("title"),
                property: "value".into(),
                sequence: Revision(2),
            },
        ]);
    assert!(matches!(
        host.apply_server_patch(&patch),
        Err(ProtocolError::DuplicateInputAcknowledgement { .. })
    ));
    assert_eq!(host.document(), &previous);
}

#[test]
fn acknowledgements_are_optional_on_the_wire_and_keep_u64_precision() {
    let original = UiPatch::new("form-1", Revision(1), Revision(2), vec![]);
    let json = serde_json::to_string(&original).unwrap();
    assert!(!json.contains("acknowledged_inputs"));
    assert_eq!(serde_json::from_str::<UiPatch>(&json).unwrap(), original);

    let acked = original.with_input_acknowledgements(vec![InputAcknowledgement {
        node_id: NodeId::from("title"),
        property: "value".into(),
        sequence: Revision(u64::MAX),
    }]);
    let encoded = serde_json::to_string(&acked).unwrap();
    assert!(encoded.contains(&u64::MAX.to_string()));
    assert_eq!(serde_json::from_str::<UiPatch>(&encoded).unwrap(), acked);
}

#[test]
fn keyed_diff_covers_removed_properties_actions_and_retyped_nodes() {
    let mut original = list_document(1, &["a", "b"]);
    let a = original
        .nodes
        .iter_mut()
        .find(|n| n.id.as_str() == "a")
        .unwrap();
    a.properties
        .insert("obsolete".into(), WireValue::from("old"));
    let mut target = original.clone();
    target.revision = Revision(2);
    let a = target
        .nodes
        .iter_mut()
        .find(|n| n.id.as_str() == "a")
        .unwrap();
    a.properties.remove("obsolete");
    a.actions.push(nulang_ui_protocol::ActionBinding {
        event: "click".into(),
        action_id: "open".into(),
        placement: nulang_ui_protocol::ActionPlacement::Client,
    });
    let b = target
        .nodes
        .iter_mut()
        .find(|n| n.id.as_str() == "b")
        .unwrap();
    b.kind = "button".into();
    let patch = original.diff(&target).unwrap();
    assert!(patch.operations.contains(&PatchOperation::RemoveProperty {
        node_id: NodeId::from("a"),
        name: "obsolete".into()
    }));
    assert!(patch.operations.iter().any(
        |op| matches!(op, PatchOperation::ReplaceActions { node_id, .. } if node_id.as_str() == "a")
    ));
    assert!(patch.operations.contains(&PatchOperation::UpsertNode {
        node: node("b", "button")
    }));
    original.apply_patch(&patch).unwrap();
    assert_eq!(original, target);
}

#[test]
fn pending_input_preserves_unacknowledged_local_edit_but_not_unrelated_server_properties() {
    let mut host = UiHostState::new(input_document()).unwrap();
    let title = NodeId::from("title");
    host.record_input(&title, "value", Revision(1), WireValue::from("typing"))
        .unwrap();
    let patch = UiPatch::new(
        "form-1",
        Revision(1),
        Revision(2),
        vec![
            PatchOperation::SetProperty {
                node_id: title.clone(),
                name: "value".into(),
                value: WireValue::from("old"),
            },
            PatchOperation::SetProperty {
                node_id: title.clone(),
                name: "error".into(),
                value: WireValue::from("required"),
            },
        ],
    );
    host.apply_server_patch(&patch).unwrap();
    let node = host
        .document()
        .nodes
        .iter()
        .find(|n| n.id == title)
        .unwrap();
    assert_eq!(node.properties["value"], WireValue::from("typing"));
    assert_eq!(node.properties["error"], WireValue::from("required"));
}

#[test]
fn removed_or_retyped_controls_discard_stale_input_state() {
    let mut host = UiHostState::new(input_document()).unwrap();
    let title = NodeId::from("title");
    host.record_input(&title, "value", Revision(1), WireValue::from("local"))
        .unwrap();
    let patch = UiPatch::new(
        "form-1",
        Revision(1),
        Revision(2),
        vec![PatchOperation::UpsertNode {
            node: node("title", "text"),
        }],
    );
    host.apply_server_patch(&patch).unwrap();
    assert!(!host
        .document()
        .nodes
        .iter()
        .find(|n| n.id == title)
        .unwrap()
        .properties
        .contains_key("value"));
    let next = UiPatch::new(
        "form-1",
        Revision(2),
        Revision(3),
        vec![PatchOperation::SetProperty {
            node_id: title.clone(),
            name: "value".into(),
            value: WireValue::from("server"),
        }],
    );
    host.apply_server_patch(&next).unwrap();
    assert_eq!(
        host.document()
            .nodes
            .iter()
            .find(|n| n.id == title)
            .unwrap()
            .properties["value"],
        WireValue::from("server")
    );
}
