use nulang_ui_protocol::{
    document_from_keyed_tree, KeyedNode, KeyedTreeError, PatchOperation, Revision, WireValue,
};

fn row(key: &str, title: &str) -> KeyedNode {
    KeyedNode::new("li")
        .with_key(key)
        .with_property("text", WireValue::from(title))
}

fn list(order: &[&str]) -> KeyedNode {
    KeyedNode::new("ul").with_children(order.iter().map(|key| row(key, key)).collect())
}

#[test]
fn identity_uses_parent_scoped_utf8_key_not_position() {
    let original = document_from_keyed_tree("tasks", Revision(1), &list(&["a", "é", "c"])).unwrap();
    let updated = document_from_keyed_tree("tasks", Revision(2), &list(&["c", "a", "é"])).unwrap();
    assert_eq!(original.root.as_str(), "root");
    assert_eq!(
        original
            .nodes
            .iter()
            .find(|n| n.id.as_str() == "root")
            .unwrap()
            .children,
        vec!["root/k61".into(), "root/kc3a9".into(), "root/k63".into()]
    );
    let patch = original.diff(&updated).unwrap();
    assert_eq!(
        patch.operations,
        vec![PatchOperation::ReplaceChildren {
            node_id: "root".into(),
            children: vec!["root/k63".into(), "root/k61".into(), "root/kc3a9".into()]
        }]
    );
}

#[test]
fn keys_are_scoped_to_ancestor_and_duplicate_siblings_fail_closed() {
    let tree = KeyedNode::new("div").with_children(vec![
        KeyedNode::new("ul")
            .with_key("left")
            .with_children(vec![row("same", "L")]),
        KeyedNode::new("ul")
            .with_key("right")
            .with_children(vec![row("same", "R")]),
    ]);
    let doc = document_from_keyed_tree("tasks", Revision(1), &tree).unwrap();
    assert!(doc
        .nodes
        .iter()
        .any(|n| n.id.as_str() == "root/k6c656674/k73616d65"));
    assert!(doc
        .nodes
        .iter()
        .any(|n| n.id.as_str() == "root/k7269676874/k73616d65"));

    let duplicate = list(&["a", "a"]);
    assert!(matches!(
        document_from_keyed_tree("tasks", Revision(1), &duplicate),
        Err(KeyedTreeError::DuplicateSiblingKey(_))
    ));
}

#[test]
fn unkeyed_positions_are_not_stable_across_reorders() {
    let tree = KeyedNode::new("ul").with_children(vec![
        KeyedNode::new("li").with_property("text", WireValue::from("a")),
        row("k", "keyed"),
        KeyedNode::new("li").with_property("text", WireValue::from("b")),
    ]);
    let doc = document_from_keyed_tree("tasks", Revision(1), &tree).unwrap();
    assert_eq!(
        doc.nodes
            .iter()
            .find(|n| n.id.as_str() == "root")
            .unwrap()
            .children,
        vec!["root/p0".into(), "root/k6b".into(), "root/p2".into()]
    );
}

#[test]
fn invalid_keys_and_excessive_depth_are_rejected() {
    assert!(matches!(
        document_from_keyed_tree("tasks", Revision(1), &list(&[""])),
        Err(KeyedTreeError::InvalidKey)
    ));
    assert!(matches!(
        document_from_keyed_tree("tasks", Revision(1), &list(&[&"x".repeat(257)])),
        Err(KeyedTreeError::InvalidKey)
    ));
    assert!(matches!(
        document_from_keyed_tree("tasks", Revision(1), &list(&["a\nb"])),
        Err(KeyedTreeError::InvalidKey)
    ));
    assert!(matches!(
        document_from_keyed_tree(
            "tasks",
            Revision(1),
            &KeyedNode::new("div").with_key("ignored")
        ),
        Err(KeyedTreeError::InvalidKey)
    ));
    let mut tree = KeyedNode::new("span");
    for _ in 0..80 {
        tree = KeyedNode::new("div").with_children(vec![tree]);
    }
    assert!(matches!(
        document_from_keyed_tree("tasks", Revision(1), &tree),
        Err(KeyedTreeError::DepthExceeded)
    ));
}
