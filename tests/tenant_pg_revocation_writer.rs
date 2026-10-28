//! Admin-only INSERT-only JTI revoker backed by disposable PostgreSQL.
//! NEVER run outside tests/run_tenant_pg_authority_ci.sh.
#![cfg(feature = "postgres")]

use nulang::web::tenant_jwt_reference::RevocationPolicy;
use nulang::web::tenant_pg_revocation_reference::DisposablePostgresRevocation;
use nulang::web::tenant_pg_revocation_writer_reference::{
    DisposablePostgresRevocationWriter, RevocationWriteError,
};
use postgres::{Config, NoTls};

fn test_port() -> u16 {
    assert_eq!(
        std::env::var("NULANG_TEST_POSTGRES_DISPOSABLE").as_deref(),
        Ok("1")
    );
    let root = std::env::var("PG_CLUSTER_CONF_ROOT").unwrap();
    assert!(root.starts_with("/tmp/pg_virtualenv.") && root.ends_with("/postgresql"));
    std::env::var("PGPORT").unwrap().parse().unwrap()
}

fn writer() -> DisposablePostgresRevocationWriter {
    DisposablePostgresRevocationWriter::new(
        "127.0.0.1",
        test_port(),
        "example_token_revoker",
        std::env::var("NULANG_TOKEN_REVOCATION_WRITER_PASSWORD").unwrap(),
    )
    .unwrap()
}

fn reader() -> DisposablePostgresRevocation {
    DisposablePostgresRevocation::new(
        "127.0.0.1",
        test_port(),
        "example_token_verifier",
        std::env::var("NULANG_TOKEN_REVOCATION_READER_PASSWORD").unwrap(),
    )
    .unwrap()
}

fn connection(user: &str, password: String) -> postgres::Client {
    let mut config = Config::new();
    config
        .host("127.0.0.1")
        .port(test_port())
        .user(user)
        .password(password)
        .dbname("postgres");
    config.connect(NoTls).unwrap()
}

#[test]
fn insert_only_writer_revokes_token_durably_and_idempotently() {
    let writer = writer();
    let r = reader();
    assert_eq!(
        r.is_revoked("issuer-a", "alice", "writer-test-a"),
        Some(false)
    );
    assert_eq!(writer.revoke("issuer-a", "alice", "writer-test-a"), Ok(()));
    assert_eq!(
        r.is_revoked("issuer-a", "alice", "writer-test-a"),
        Some(true)
    );
    assert_eq!(writer.revoke("issuer-a", "alice", "writer-test-a"), Ok(()));
    assert_eq!(
        reader().is_revoked("issuer-a", "alice", "writer-test-a"),
        Some(true)
    );
    assert_eq!(
        r.is_revoked("issuer-a", "bob", "writer-test-a"),
        Some(false)
    );
}

#[test]
fn writer_has_no_select_update_delete_or_tenant_data_access() {
    let mut c = connection(
        "example_token_revoker",
        std::env::var("NULANG_TOKEN_REVOCATION_WRITER_PASSWORD").unwrap(),
    );
    assert!(c
        .query("SELECT * FROM tenant_example_private.revoked_tokens", &[])
        .is_err());
    assert!(c
        .execute("DELETE FROM tenant_example_private.revoked_tokens", &[])
        .is_err());
    assert!(c
        .execute(
            "UPDATE tenant_example_private.revoked_tokens SET jti = 'bad'",
            &[]
        )
        .is_err());
    assert!(c
        .query("SELECT * FROM tenant_example.billboard_faces", &[])
        .is_err());

    // Tenant login can neither read nor revoke any token.
    let mut tenant = connection(
        "example_tenant_a",
        std::env::var("NULANG_TENANT_TEST_PASSWORD_A").unwrap(),
    );
    assert!(tenant
        .query("SELECT * FROM tenant_example_private.revoked_tokens", &[])
        .is_err());
    assert!(tenant.execute(
        "INSERT INTO tenant_example_private.revoked_tokens(issuer,subject,jti) VALUES ('issuer-a','alice','tamper')", &[],
    ).is_err());
}

#[test]
fn invalid_or_unavailable_writer_never_reports_success() {
    assert!(DisposablePostgresRevocationWriter::new(
        "database.prod",
        15433,
        "example_token_revoker",
        "secret".into()
    )
    .is_none());
    assert!(DisposablePostgresRevocationWriter::new(
        "127.0.0.1",
        5432,
        "example_token_revoker",
        "secret".into()
    )
    .is_none());
    assert!(DisposablePostgresRevocationWriter::new(
        "127.0.0.1",
        test_port(),
        "example_tenant_a",
        "secret".into()
    )
    .is_none());
    let write = writer();
    for (iss, sub, jti) in [
        ("", "alice", "a"),
        ("issuer-a", "", "a"),
        ("issuer-a", "alice", ""),
        ("issuer-a", "alice", "bad\nvalue"),
    ] {
        assert_eq!(
            write.revoke(iss, sub, jti),
            Err(RevocationWriteError::InvalidInput)
        );
    }
    let offline = DisposablePostgresRevocationWriter::new(
        "127.0.0.1",
        test_port() + 1,
        "example_token_revoker",
        std::env::var("NULANG_TOKEN_REVOCATION_WRITER_PASSWORD").unwrap(),
    )
    .unwrap();
    assert_eq!(
        offline.revoke("issuer-a", "alice", "offline-jti"),
        Err(RevocationWriteError::Unavailable)
    );
}

#[test]
fn revoked_insert_privilege_fails_closed_and_recovers_only_after_regrant() {
    let privileged = |sql: &str| {
        let status = std::process::Command::new("psql")
            .args([
                "-w",
                "-X",
                "-v",
                "ON_ERROR_STOP=1",
                "-d",
                "postgres",
                "-c",
                sql,
            ])
            .status()
            .expect("disposable pg_virtualenv psql available");
        assert!(status.success());
    };
    let writer = writer();
    privileged("REVOKE INSERT ON tenant_example_private.revoked_tokens FROM example_token_revoker");
    assert_eq!(
        writer.revoke("issuer-a", "alice", "writer-no-insert"),
        Err(RevocationWriteError::Unavailable)
    );
    assert_eq!(
        reader().is_revoked("issuer-a", "alice", "writer-no-insert"),
        Some(false)
    );
    privileged("GRANT INSERT ON tenant_example_private.revoked_tokens TO example_token_revoker");
    assert_eq!(
        writer.revoke("issuer-a", "alice", "writer-no-insert"),
        Ok(())
    );
    assert_eq!(
        reader().is_revoked("issuer-a", "alice", "writer-no-insert"),
        Some(true)
    );
}
