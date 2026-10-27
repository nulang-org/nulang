#![cfg(feature = "turso-remote-probe")]
use nulang::runtime::TursoRemotePreflight;
use std::io::ErrorKind;

#[test]
fn accepts_only_encrypted_remote_database_addresses() {
    for url in [
        "libsql://example.turso.io",
        "turso://example.turso.io",
        "https://example.turso.io/",
    ] {
        let check = TursoRemotePreflight::new(url, "placeholder-token");
        assert!(check.is_ok(), "should accept encrypted URL: {url}");
    }
    for url in [
        "http://example.turso.io",
        "http://localhost:8080",
        "file:///tmp/database.db",
        "https://user:secret@example.turso.io",
        "https://example.turso.io/?token=secret",
        "https://example.turso.io/#fragment",
        "https://example.turso.io/some/path",
        "https://",
        "not a url",
    ] {
        let result = TursoRemotePreflight::new(url, "placeholder-token");
        assert_eq!(
            result.unwrap_err().kind(),
            ErrorKind::InvalidInput,
            "accepted {url}"
        );
    }
}

#[test]
fn requires_nonempty_credentials_and_redacts_debug_output() {
    assert_eq!(
        TursoRemotePreflight::new("https://example.turso.io", "")
            .unwrap_err()
            .kind(),
        ErrorKind::PermissionDenied,
    );
    let preflight = TursoRemotePreflight::new("https://example.turso.io", "unique-secret-427")
        .expect("valid TLS URL");
    let debug = format!("{preflight:?}");
    assert!(!debug.contains("unique-secret-427"));
    assert!(debug.contains("example.turso.io"));
}

#[test]
fn never_log_token_in_rejection_messages() {
    let token = "do-not-log-this-token";
    let err = TursoRemotePreflight::new("http://db.example.net", token).unwrap_err();
    assert!(!err.to_string().contains(token));
}
