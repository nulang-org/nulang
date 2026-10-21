//! Regression coverage for unambiguous UI events across renderer hosts.
//!
//! A node may handle multiple *different* events. Each event must have one
//! binding on that node so a DOM/SwiftUI/Compose adapter cannot disagree about
//! which handler to invoke.

use nulang_ui_protocol::{
    ActionBinding, ActionId, ActionPlacement, NodeId, PatchOperation, Revision, UiDocument, UiNode,
    UiPatch,
};

fn binding(event: &str, action_id: &str) -> ActionBinding {
    ActionBinding {
        event: event.to_owned(),
        action_id: ActionId::from(action_id),
        placement: ActionPlacement::Client,
    }
}

#[test]
fn test_duplicate_event_on_one_node_is_rejected() {
    let mut button = UiNode::new("submit", "button");
    button.actions = vec![binding("activate", "save"), binding("activate", "delete")];
    let document = UiDocument::new("form", Revision(1), "submit", vec![button]);

    let error = document
        .validate()
        .expect_err("two handlers for one node event must fail before entering any renderer");
    assert!(
        error.to_string().contains("duplicate action event"),
        "unexpected validation error: {error}"
    );
}

#[test]
fn test_patch_cannot_install_ambiguous_actions_or_mutate_the_document() {
    let mut button = UiNode::new("submit", "button");
    button.actions.push(binding("activate", "save"));
    let mut document = UiDocument::new("form", Revision(1), "submit", vec![button]);
    let before = document.clone();

    let patch = UiPatch::new(
        "form",
        Revision(1),
        Revision(2),
        vec![PatchOperation::ReplaceActions {
            node_id: NodeId::from("submit"),
            actions: vec![binding("activate", "save"), binding("activate", "delete")],
        }],
    );

    assert!(document.apply_patch(&patch).is_err());
    assert_eq!(document, before, "rejected patch must be atomic");
}

#[test]
fn test_different_events_and_different_nodes_are_independent() {
    let mut root = UiNode::new("root", "column");
    root.children = vec![NodeId::from("save"), NodeId::from("cancel")];

    let mut save = UiNode::new("save", "button");
    save.actions = vec![
        binding("activate", "save"),
        binding("focus", "record_focus"),
    ];

    let mut cancel = UiNode::new("cancel", "button");
    cancel.actions.push(binding("activate", "cancel"));

    let document = UiDocument::new("form", Revision(1), "root", vec![root, save, cancel]);
    document.validate().unwrap();
}
