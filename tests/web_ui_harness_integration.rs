//! Prove compiled Nulang views and the new browser-free protocol harness agree.
//! This does not authorize any production HTTP/SSE transport.
use nulang::ast::AstModule;
use nulang::lexer::Lexer;
use nulang::parser::Parser;
use nulang::web::ui_live_session::{UiInputCommit, UiLiveSession, UiSessionError, UiSessionScope};
use nulang::web::ui_template::{UiTemplate, UiTemplateManifest};
use nulang_ui_protocol::{
    testing::{UiTestError, UiTestHarness},
    NodeId, Revision, RuntimeToHostMessage, WireValue,
};
use std::collections::BTreeMap;
use std::sync::atomic::{AtomicBool, Ordering};

fn compile_view(name: &str, source: &str) -> UiTemplateManifest {
    let ast: AstModule = Parser::new(Lexer::new(source).lex().unwrap())
        .parse_module()
        .unwrap();
    let view = UiTemplate::compile(&ast, name).unwrap();
    UiTemplateManifest {
        version: 1,
        templates: BTreeMap::from([(name.to_owned(), view)]),
    }
}

fn scope() -> UiSessionScope {
    UiSessionScope::new("tenant-7", "user-12", "session-123", "editor-doc")
}

fn authorized(scope: &UiSessionScope) -> bool {
    scope.tenant == "tenant-7"
        && scope.subject == "user-12"
        && scope.session == "session-123"
        && scope.document_id.as_str() == "editor-doc"
        && matches!(scope.view.as_str(), "list" | "edit")
}

fn rows(ids: &[&str]) -> BTreeMap<String, WireValue> {
    let values = ids
        .iter()
        .map(|id| {
            WireValue::Object(BTreeMap::from([
                ("id".to_owned(), WireValue::from(*id)),
                ("label".to_owned(), WireValue::from(*id)),
            ]))
        })
        .collect();
    BTreeMap::from([("rows".to_owned(), WireValue::Array(values))])
}

#[test]
fn compiled_keyed_view_reconciles_with_authorized_session_snapshots() {
    let manifest = compile_view(
        "list",
        r#"
import stdlib::web::html
import stdlib::web::types
fn list() -> Html {
    <ul>{for item in rows { <li key={item.id}>{item.label}</li> }}</ul>
}
"#,
    );
    let mut server =
        UiLiveSession::open(&manifest, "list", scope(), &rows(&["a", "b"]), &authorized).unwrap();
    let mut browser_free = UiTestHarness::mount(server.snapshot(&authorized).unwrap()).unwrap();
    let before = browser_free.document().clone();
    assert_eq!(before.revision, Revision(1));

    // A valid server render emits the real keyed UI patch and updates the host.
    let patch = server
        .commit(&rows(&["b", "a", "c"]), &[], &authorized, &|_| true)
        .unwrap();
    browser_free.receive(patch.clone()).unwrap();
    let RuntimeToHostMessage::Patch { patch, .. } = patch else {
        panic!("expected patch");
    };
    assert_eq!(patch.base_revision, Revision(1));
    assert_eq!(patch.revision, Revision(2));
    assert_ne!(browser_free.document().nodes, before.nodes);

    let RuntimeToHostMessage::Snapshot {
        document: authoritative,
        ..
    } = server.snapshot(&authorized).unwrap()
    else {
        panic!("expected snapshot");
    };
    assert_eq!(browser_free.document(), &authoritative);
    assert_eq!(browser_free.document().revision, Revision(2));
    // Replaying an identical snapshot after reconnection is idempotent.
    browser_free
        .receive(RuntimeToHostMessage::snapshot(authoritative))
        .unwrap();
    assert!(matches!(
        browser_free.receive(RuntimeToHostMessage::snapshot(before)),
        Err(UiTestError::StaleSnapshot { .. })
    ));
}

#[test]
fn rejected_form_edits_never_advance_server_or_test_host() {
    let manifest = compile_view(
        "edit",
        r#"
import stdlib::web::html
import stdlib::web::types
fn edit() -> Html { <form><input value={title}/></form> }
"#,
    );
    let initial = BTreeMap::from([("title".to_owned(), WireValue::from(""))]);
    let next = BTreeMap::from([("title".to_owned(), WireValue::from("approved"))]);
    let mut server =
        UiLiveSession::open(&manifest, "edit", scope(), &initial, &authorized).unwrap();
    let mut client = UiTestHarness::mount(server.snapshot(&authorized).unwrap()).unwrap();
    let starting_document = client.document().clone();
    let input = UiInputCommit {
        node_id: NodeId::from("root/p0"),
        property: "value".into(),
        sequence: Revision(1),
        value: WireValue::from("approved"),
    };

    assert!(matches!(
        server.commit(&next, &[input.clone()], &authorized, &|_| false),
        Err(UiSessionError::ServerRejected)
    ));
    assert_eq!(server.revision(), Revision(1));
    assert_eq!(client.document(), &starting_document);

    client
        .receive(
            server
                .commit(&next, &[input.clone()], &authorized, &|items| {
                    items == [input.clone()]
                })
                .unwrap(),
        )
        .unwrap();
    assert_eq!(client.document().revision, Revision(2));
    assert_eq!(
        client.node("root/p0").unwrap().properties["value"],
        WireValue::from("approved")
    );
    assert!(matches!(
        server.commit(&next, &[input], &authorized, &|_| true),
        Err(UiSessionError::Replay)
    ));
    assert_eq!(server.revision(), client.document().revision);
}

#[test]
fn revocation_blocks_live_session_output_without_mutating_test_host() {
    let manifest = compile_view(
        "edit",
        r#"
import stdlib::web::html
import stdlib::web::types
fn edit() -> Html { <form><input value={title}/></form> }
"#,
    );
    let bindings = BTreeMap::from([("title".to_owned(), WireValue::from("draft"))]);
    let active = AtomicBool::new(true);
    let verify = |s: &UiSessionScope| active.load(Ordering::SeqCst) && authorized(s);
    let mut server = UiLiveSession::open(&manifest, "edit", scope(), &bindings, &verify).unwrap();
    let host = UiTestHarness::mount(server.snapshot(&verify).unwrap()).unwrap();
    let before = host.document().clone();

    active.store(false, Ordering::SeqCst);
    assert!(matches!(
        server.snapshot(&verify),
        Err(UiSessionError::Unauthorized)
    ));
    assert!(matches!(
        server.commit(&bindings, &[], &verify, &|_| true),
        Err(UiSessionError::Unauthorized)
    ));
    assert_eq!(host.document(), &before);
    assert_eq!(server.revision(), before.revision);

    active.store(true, Ordering::SeqCst);
    server.revoke();
    assert!(matches!(
        server.snapshot(&verify),
        Err(UiSessionError::Revoked)
    ));
    assert_eq!(host.document(), &before);
}
