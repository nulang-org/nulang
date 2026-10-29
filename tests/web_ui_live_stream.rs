use nulang::ast::AstModule;
use nulang::lexer::Lexer;
use nulang::parser::Parser;
use nulang::web::ui_live_session::{UiLiveSession, UiSessionError, UiSessionScope};
use nulang::web::ui_live_stream::{UiStreamDelivery, UiStreamError};
use nulang::web::ui_template::{UiTemplate, UiTemplateManifest};
use nulang_ui_protocol::{Revision, RuntimeToHostMessage, WireValue};
use std::collections::BTreeMap;
use std::sync::atomic::{AtomicBool, Ordering};

fn manifest() -> UiTemplateManifest {
    let ast: AstModule = Parser::new(
        Lexer::new(
            r#"
import stdlib::web::types
fn tasks() -> Html { <ul>{for item in rows { <li key={item.id}>{item.label}</li> }}</ul> }
"#,
        )
        .lex()
        .unwrap(),
    )
    .parse_module()
    .unwrap();
    UiTemplateManifest {
        version: 1,
        templates: BTreeMap::from([("tasks".into(), UiTemplate::compile(&ast, "tasks").unwrap())]),
    }
}
fn rows(names: &[&str]) -> BTreeMap<String, WireValue> {
    BTreeMap::from([(
        "rows".into(),
        WireValue::Array(
            names
                .iter()
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
fn open() -> UiLiveSession {
    UiLiveSession::open(
        &manifest(),
        "tasks",
        UiSessionScope::new("t", "u", "s", "d"),
        &rows(&["a", "b"]),
        &|_| true,
    )
    .unwrap()
}

#[test]
fn authenticated_snapshot_uses_typed_wire_format_and_revision_sse_id() {
    let server = open();
    let frame = UiStreamDelivery::subscribe(&server, None, &|_| true).unwrap();
    assert!(
        frame.starts_with("event: nulang-ui\nid: 1\ndata: "),
        "{frame}"
    );
    assert!(frame.ends_with("\n\n"), "{frame}");
    let payload = frame
        .lines()
        .find_map(|line| line.strip_prefix("data: "))
        .unwrap();
    let decoded: RuntimeToHostMessage = serde_json::from_str(payload).unwrap();
    assert!(matches!(decoded, RuntimeToHostMessage::Snapshot { .. }));
    decoded.validate().unwrap();
}

#[test]
fn patch_event_is_revisioned_and_cross_document_payload_fails_closed() {
    let mut server = open();
    let event = server
        .commit(&rows(&["b", "a"]), &[], &|_| true, &|_| true)
        .unwrap();
    let frame = UiStreamDelivery::publish(&server, &event, &|_| true).unwrap();
    assert!(
        frame.starts_with("event: nulang-ui\nid: 2\ndata: "),
        "{frame}"
    );
    let payload = frame
        .lines()
        .find_map(|line| line.strip_prefix("data: "))
        .unwrap();
    assert!(matches!(
        serde_json::from_str::<RuntimeToHostMessage>(payload).unwrap(),
        RuntimeToHostMessage::Patch { .. }
    ));
    let mut forged = event.clone();
    if let RuntimeToHostMessage::Patch { patch, .. } = &mut forged {
        patch.operations.clear();
    }
    assert!(matches!(
        UiStreamDelivery::publish(&server, &forged, &|_| true),
        Err(UiStreamError::InvalidMessage)
    ));
    let mut tampered = event.clone();
    if let RuntimeToHostMessage::Patch { patch, .. } = &mut tampered {
        patch.document_id = "other".into();
    }
    assert!(matches!(
        UiStreamDelivery::publish(&server, &tampered, &|_| true),
        Err(UiStreamError::DocumentMismatch)
    ));
}

#[test]
fn revocation_and_failed_authorization_block_subscription_and_each_event() {
    let mut server = open();
    let allowed = AtomicBool::new(true);
    let auth = |_: &nulang::web::ui_live_session::UiSessionScope| allowed.load(Ordering::SeqCst);
    UiStreamDelivery::subscribe(&server, None, &auth).unwrap();
    allowed.store(false, Ordering::SeqCst);
    assert!(matches!(
        UiStreamDelivery::subscribe(&server, None, &auth),
        Err(UiStreamError::Session(UiSessionError::Unauthorized))
    ));
    let msg = server.snapshot(&|_| true).unwrap();
    assert!(matches!(
        UiStreamDelivery::publish(&server, &msg, &auth),
        Err(UiStreamError::Session(UiSessionError::Unauthorized))
    ));
    allowed.store(true, Ordering::SeqCst);
    server.revoke();
    assert!(matches!(
        UiStreamDelivery::subscribe(&server, None, &auth),
        Err(UiStreamError::Session(UiSessionError::Revoked))
    ));
}

#[test]
fn reconnect_rejects_future_cursor_and_sends_fresh_snapshot_for_gaps() {
    let mut server = open();
    assert!(matches!(
        UiStreamDelivery::subscribe(&server, Some("2"), &|_| true),
        Err(UiStreamError::FutureCursor)
    ));
    server
        .commit(&rows(&["b", "a"]), &[], &|_| true, &|_| true)
        .unwrap();
    assert!(UiStreamDelivery::subscribe(&server, Some("1"), &|_| true)
        .unwrap()
        .contains("\"revision\":\"2\""));
    assert!(matches!(
        UiStreamDelivery::subscribe(&server, Some("garbage"), &|_| true),
        Err(UiStreamError::InvalidCursor)
    ));
    assert!(UiStreamDelivery::subscribe(&server, Some("2"), &|_| true)
        .unwrap()
        .starts_with("event: nulang-ui\nid: 2\n"));
}

#[test]
fn no_trusted_event_replay_from_other_document_or_old_revision() {
    let mut server = open();
    let prior = server.snapshot(&|_| true).unwrap();
    server
        .commit(&rows(&["b", "a"]), &[], &|_| true, &|_| true)
        .unwrap();
    assert!(matches!(
        UiStreamDelivery::publish(&server, &prior, &|_| true),
        Err(UiStreamError::RevisionMismatch)
    ));
    assert_eq!(server.revision(), Revision(2));
}
