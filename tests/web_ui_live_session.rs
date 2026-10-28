use nulang::ast::AstModule;
use nulang::lexer::Lexer;
use nulang::parser::Parser;
use nulang::web::ui_live_session::{UiInputCommit, UiLiveSession, UiSessionError, UiSessionScope};
use nulang::web::ui_template::{UiTemplate, UiTemplateManifest};
use nulang_ui_protocol::{
    NodeId, PatchApplyOutcome, Revision, RuntimeToHostMessage, UiHostState, WireValue,
};
use std::collections::BTreeMap;
use std::sync::atomic::{AtomicBool, Ordering};

fn template() -> UiTemplate {
    let source = r#"
import stdlib::web::html
import stdlib::web::types
fn view() -> Html {
    <ul>{for item in rows { <li key={item.id}>{item.label}</li> }}</ul>
}
"#;
    let ast: AstModule = Parser::new(Lexer::new(source).lex().unwrap())
        .parse_module()
        .unwrap();
    UiTemplate::compile(&ast, "view").unwrap()
}

fn rows(ids: &[&str]) -> BTreeMap<String, WireValue> {
    BTreeMap::from([(
        "rows".into(),
        WireValue::Array(
            ids.iter()
                .map(|id| {
                    WireValue::Object(BTreeMap::from([
                        ("id".into(), WireValue::from(*id)),
                        ("label".into(), WireValue::from(*id)),
                    ]))
                })
                .collect(),
        ),
    )])
}

fn scope() -> UiSessionScope {
    UiSessionScope::new("tenant-7", "user-12", "session-123", "tasks")
}

#[test]
fn authorized_compiled_template_delivers_snapshot_and_keyed_patch() {
    let manifest = UiTemplateManifest {
        version: 1,
        templates: BTreeMap::from([("view".into(), template())]),
    };
    let mut server =
        UiLiveSession::open(&manifest, "view", scope(), &rows(&["a", "b"]), &|_| true).unwrap();
    let RuntimeToHostMessage::Snapshot { document, .. } = server.snapshot(&|_| true).unwrap()
    else {
        panic!("expected snapshot")
    };
    assert_eq!(document.revision, Revision(1));
    let mut client = UiHostState::new(document.clone()).unwrap();

    let RuntimeToHostMessage::Patch { patch, .. } = server
        .commit(&rows(&["b", "a"]), &[], &|_| true, &|_| true)
        .unwrap()
    else {
        panic!("expected patch")
    };
    assert_eq!(patch.base_revision, Revision(1));
    assert_eq!(patch.revision, Revision(2));
    assert_eq!(patch.operations.len(), 1);
    assert_eq!(
        client.apply_server_patch(&patch).unwrap(),
        PatchApplyOutcome::Applied
    );
    let RuntimeToHostMessage::Snapshot {
        document: authoritative,
        ..
    } = server.snapshot(&|_| true).unwrap()
    else {
        panic!("expected snapshot")
    };
    assert_eq!(client.document(), &authoritative);
}

#[test]
fn revoked_or_invalid_sessions_cannot_read_or_mutate_templates() {
    let manifest = UiTemplateManifest {
        version: 1,
        templates: BTreeMap::from([("view".into(), template())]),
    };
    assert!(matches!(
        UiLiveSession::open(&manifest, "view", scope(), &rows(&["a"]), &|_| false),
        Err(UiSessionError::Unauthorized)
    ));
    let active = AtomicBool::new(true);
    let permitted = |request: &UiSessionScope| {
        active.load(Ordering::SeqCst)
            && request.tenant == "tenant-7"
            && request.subject == "user-12"
    };
    let mut server =
        UiLiveSession::open(&manifest, "view", scope(), &rows(&["a"]), &permitted).unwrap();
    active.store(false, Ordering::SeqCst);
    assert!(matches!(
        server.snapshot(&permitted),
        Err(UiSessionError::Unauthorized)
    ));
    let previous = server.revision();
    assert!(matches!(
        server.commit(&rows(&["b"]), &[], &permitted, &|_| true),
        Err(UiSessionError::Unauthorized)
    ));
    assert_eq!(previous, server.revision());
}

#[test]
fn failed_render_or_server_rejection_never_advances_revision() {
    let manifest = UiTemplateManifest {
        version: 1,
        templates: BTreeMap::from([("view".into(), template())]),
    };
    let mut server =
        UiLiveSession::open(&manifest, "view", scope(), &rows(&["a"]), &|_| true).unwrap();
    assert!(matches!(
        server.commit(&BTreeMap::new(), &[], &|_| true, &|_| true),
        Err(UiSessionError::Template(_))
    ));
    assert_eq!(server.revision(), Revision(1));

    let accepted = UiInputCommit {
        node_id: NodeId::from("root/k61"),
        property: "value".into(),
        sequence: Revision(1),
        value: WireValue::from("x"),
    };
    assert!(matches!(
        server.commit(&rows(&["a"]), &[accepted], &|_| true, &|_| false),
        Err(UiSessionError::InvalidInput(_))
    ));
    assert_eq!(server.revision(), Revision(1));
}

#[test]
fn acknowledgements_require_trusted_acceptance_and_reject_replayed_sequences() {
    let src = r#"
import stdlib::web::html
import stdlib::web::types
fn edit() -> Html {
  <form><input value={title}/></form>
}
"#;
    let ast = Parser::new(Lexer::new(src).lex().unwrap())
        .parse_module()
        .unwrap();
    let manifest = UiTemplateManifest {
        version: 1,
        templates: BTreeMap::from([("edit".into(), UiTemplate::compile(&ast, "edit").unwrap())]),
    };
    let initial = BTreeMap::from([("title".into(), WireValue::from(""))]);
    let mut server = UiLiveSession::open(&manifest, "edit", scope(), &initial, &|_| true).unwrap();
    let old = server.revision();
    let input = UiInputCommit {
        node_id: NodeId::from("root/p0"),
        property: "value".into(),
        sequence: Revision(1),
        value: WireValue::from("typed"),
    };
    let updated = BTreeMap::from([("title".into(), WireValue::from("typed"))]);
    assert!(matches!(
        server.commit(&updated, &[input.clone()], &|_| true, &|_| false),
        Err(UiSessionError::ServerRejected)
    ));
    assert_eq!(server.revision(), old);
    let RuntimeToHostMessage::Patch { patch, .. } = server
        .commit(&updated, &[input.clone()], &|_| true, &|items| {
            items.len() == 1
        })
        .unwrap()
    else {
        panic!("missing patch")
    };
    assert_eq!(patch.acknowledged_inputs.len(), 1);
    assert_eq!(patch.acknowledged_inputs[0].sequence, Revision(1));
    assert!(matches!(
        server.commit(&updated, &[input], &|_| true, &|_| true),
        Err(UiSessionError::Replay)
    ));
    assert_eq!(server.revision(), Revision(2));
}

#[test]
fn corrupt_manifest_or_cross_document_scope_never_opens() {
    let manifest = UiTemplateManifest {
        version: 2,
        templates: BTreeMap::from([("view".into(), template())]),
    };
    assert!(matches!(
        UiLiveSession::open(&manifest, "view", scope(), &rows(&["a"]), &|_| true),
        Err(UiSessionError::UnsupportedManifest)
    ));
    let manifest = UiTemplateManifest {
        version: 1,
        templates: BTreeMap::from([("view".into(), template())]),
    };
    assert!(matches!(
        UiLiveSession::open(
            &manifest,
            "view",
            UiSessionScope::new("", "user", "session", "doc"),
            &rows(&["a"]),
            &|_| true
        ),
        Err(UiSessionError::InvalidScope)
    ));
}

#[test]
fn server_can_open_private_compiled_manifest_without_client_template_selection() {
    let manifest = UiTemplateManifest {
        version: 1,
        templates: BTreeMap::from([("view".into(), template())]),
    };
    let dir =
        std::env::temp_dir().join(format!("nulang-ui-session-manifest-{}", std::process::id()));
    std::fs::create_dir_all(&dir).unwrap();
    let path = dir.join("app.ui-templates.json");
    std::fs::write(&path, serde_json::to_vec(&manifest).unwrap()).unwrap();
    let mut session = UiLiveSession::open_compiled_manifest(
        &path,
        "view",
        scope(),
        &rows(&["a", "b"]),
        &|scope| scope.view == "view" && scope.tenant == "tenant-7",
    )
    .unwrap();
    assert!(matches!(
        session.snapshot(&|s| s.view != "view"),
        Err(UiSessionError::Unauthorized)
    ));
    assert_eq!(session.revision(), Revision(1));
    session.revoke();
    assert!(matches!(
        session.snapshot(&|_| true),
        Err(UiSessionError::Revoked)
    ));
    let _ = std::fs::remove_dir_all(dir);
}

#[test]
fn malformed_or_oversized_manifest_is_rejected_before_session_creation() {
    let dir =
        std::env::temp_dir().join(format!("nulang-ui-session-invalid-{}", std::process::id()));
    std::fs::create_dir_all(&dir).unwrap();
    let path = dir.join("app.ui-templates.json");
    std::fs::write(&path, vec![b'x'; 2_100_000]).unwrap();
    assert!(matches!(
        UiLiveSession::open_compiled_manifest(&path, "view", scope(), &rows(&["a"]), &|_| true),
        Err(UiSessionError::InvalidArtifact)
    ));
    std::fs::write(&path, b"{not-json").unwrap();
    assert!(matches!(
        UiLiveSession::open_compiled_manifest(&path, "view", scope(), &rows(&["a"]), &|_| true),
        Err(UiSessionError::InvalidArtifact)
    ));
    let _ = std::fs::remove_dir_all(dir);
}

#[test]
fn revocation_during_input_acceptance_blocks_patch_commit() {
    let src = r#"
import stdlib::web::types
fn edit() -> Html { <form><input value={title}/></form> }
"#;
    let ast = Parser::new(Lexer::new(src).lex().unwrap())
        .parse_module()
        .unwrap();
    let manifest = UiTemplateManifest {
        version: 1,
        templates: BTreeMap::from([("edit".into(), UiTemplate::compile(&ast, "edit").unwrap())]),
    };
    let active = AtomicBool::new(true);
    let policy = |_: &UiSessionScope| active.load(Ordering::SeqCst);
    let mut server = UiLiveSession::open(
        &manifest,
        "edit",
        scope(),
        &BTreeMap::from([("title".into(), WireValue::from(""))]),
        &policy,
    )
    .unwrap();
    let input = UiInputCommit {
        node_id: NodeId::from("root/p0"),
        property: "value".into(),
        sequence: Revision(1),
        value: WireValue::from("new"),
    };
    let acceptance = |_: &[UiInputCommit]| {
        active.store(false, Ordering::SeqCst);
        true
    };
    assert!(matches!(
        server.commit(
            &BTreeMap::from([("title".into(), WireValue::from("new"))]),
            &[input],
            &policy,
            &acceptance
        ),
        Err(UiSessionError::Unauthorized)
    ));
    assert_eq!(server.revision(), Revision(1));
}
