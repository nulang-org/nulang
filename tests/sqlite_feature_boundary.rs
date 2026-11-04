//! External API contract: the default local-only build must fail closed for remote SQL.

#[cfg(all(feature = "sqlite", not(feature = "sqlite-remote")))]
#[test]
fn remote_libsql_requires_explicit_feature_before_using_credentials() {
    use nulang::runtime::LibsqlStore;
    let err = LibsqlStore::new_remote("http://127.0.0.1:1", "fake-test-token")
        .err()
        .expect("remote should be disabled without sqlite-remote");
    assert_eq!(err.kind(), std::io::ErrorKind::Unsupported);
    assert!(err.to_string().contains("sqlite-remote"));
}
