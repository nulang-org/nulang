//! The no-SQLite build must reject an explicit web persistence request.
#![cfg(not(feature = "sqlite"))]

use nulang::runtime::WebDevServer;

#[test]
fn explicit_sqlite_web_state_is_unsupported_without_sqlite_feature() {
    let result = WebDevServer::bind_runtime_with_state(
        0,
        None,
        None,
        vec![],
        Some("data/state.sqlite".into()),
    );
    let error = result.expect_err("requested SQLite must not degrade to memory");
    assert_eq!(error.kind(), std::io::ErrorKind::Unsupported);
}
