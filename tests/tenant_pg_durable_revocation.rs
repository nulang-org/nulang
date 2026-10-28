//! Durable, fail-closed revocation admission against ephemeral PostgreSQL.
//! Must run under tests/run_tenant_pg_authority_ci.sh only.
#![cfg(feature = "postgres")]

use base64::{engine::general_purpose::URL_SAFE_NO_PAD, Engine as _};
use ed25519_dalek::{Signer, SigningKey};
use nulang::web::tenant_jwt_reference::{PinnedEd25519Verifier, RevocationPolicy};
use nulang::web::tenant_pg_reference::TrustedTokenVerifier;
use nulang::web::tenant_pg_revocation_reference::DisposablePostgresRevocation;
use serde_json::json;
use std::process::Command;
use std::time::{SystemTime, UNIX_EPOCH};

fn isolated_port() -> u16 {
    assert_eq!(
        std::env::var("NULANG_TEST_POSTGRES_DISPOSABLE").as_deref(),
        Ok("1"),
        "must never contact a non-disposable database"
    );
    let config = std::env::var("PG_CLUSTER_CONF_ROOT").unwrap();
    assert!(config.starts_with("/tmp/pg_virtualenv.") && config.ends_with("/postgresql"));
    let port = std::env::var("PGPORT").unwrap().parse::<u16>().unwrap();
    assert_ne!(port, 5432);
    port
}

fn revocations() -> DisposablePostgresRevocation {
    DisposablePostgresRevocation::new(
        "127.0.0.1",
        isolated_port(),
        "example_token_verifier",
        std::env::var("NULANG_TOKEN_REVOCATION_READER_PASSWORD").unwrap(),
    )
    .expect("guarded local revocation reader")
}

fn signer() -> SigningKey {
    SigningKey::from_bytes(&[71u8; 32]) // deterministic test-only signing key
}

fn signed(subject: &str, jti: &str) -> String {
    let now = i64::try_from(
        SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .unwrap()
            .as_secs(),
    )
    .unwrap();
    let head = URL_SAFE_NO_PAD.encode(
        serde_json::to_vec(&json!({"alg":"EdDSA","typ":"at+jwt","kid":"revocation-test-key"}))
            .unwrap(),
    );
    let payload = URL_SAFE_NO_PAD.encode(
        serde_json::to_vec(&json!({
            "iss":"issuer-a", "sub":subject, "aud":"nulang-tenant-api",
            "exp":now+120, "iat":now-2, "nbf":now-2, "jti":jti
        }))
        .unwrap(),
    );
    let unsigned = format!("{head}.{payload}");
    let sig = signer().sign(unsigned.as_bytes());
    format!("{unsigned}.{}", URL_SAFE_NO_PAD.encode(sig.to_bytes()))
}

fn verifier() -> PinnedEd25519Verifier<DisposablePostgresRevocation> {
    PinnedEd25519Verifier::new(
        "issuer-a",
        "nulang-tenant-api",
        "revocation-test-key",
        signer().verifying_key(),
        revocations(),
    )
    .unwrap()
}

fn privileged_sql(sql: &str) {
    // Uses the pg_virtualenv process's ephemeral privileged LOGIN, never
    // production credentials. SQL strings below are hardcoded test fixtures.
    let status = Command::new("psql")
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
        .expect("psql available in disposable wrapper");
    assert!(status.success(), "disposable setup SQL failed");
}

#[test]
fn signed_access_token_revocation_is_persistent_and_effective_at_next_request() {
    let token = signed("alice", "durable-revocation-test-1");
    let bob = signed("bob", "durable-revocation-bob");
    let v = verifier();
    assert_eq!(v.verify(&token), Some("issuer-a|alice".into()));
    assert_eq!(v.verify(&bob), Some("issuer-a|bob".into()));
    privileged_sql("INSERT INTO tenant_example_private.revoked_tokens(issuer,subject,jti) VALUES ('issuer-a','alice','durable-revocation-test-1')");
    assert_eq!(
        v.verify(&token),
        None,
        "revoked valid signature must deny immediately"
    );
    assert_eq!(
        verifier().verify(&token),
        None,
        "fresh verifier must observe persisted revocation"
    );
    assert_eq!(
        v.verify(&bob),
        Some("issuer-a|bob".into()),
        "revocation cannot spill to another principal"
    );
    assert_eq!(
        revocations().is_revoked("issuer-a", "alice", "durable-revocation-test-1"),
        Some(true)
    );
    assert_eq!(
        revocations().is_revoked("issuer-a", "bob", "durable-revocation-test-1"),
        Some(false)
    );
}

#[test]
fn database_privilege_outage_and_unreachable_host_deny_instead_of_allow() {
    let token = signed("alice", "durable-revocation-fail-closed");
    assert_eq!(verifier().verify(&token), Some("issuer-a|alice".into()));
    privileged_sql(
        "REVOKE SELECT ON tenant_example_private.revoked_tokens FROM example_token_verifier",
    );
    assert_eq!(
        verifier().verify(&token),
        None,
        "missing SELECT is not unrevoked"
    );
    assert_eq!(
        revocations().is_revoked("issuer-a", "alice", "unknown"),
        None
    );
    privileged_sql(
        "GRANT SELECT ON tenant_example_private.revoked_tokens TO example_token_verifier",
    );
    assert_eq!(verifier().verify(&token), Some("issuer-a|alice".into()));
    let disconnected = DisposablePostgresRevocation::new(
        "127.0.0.1",
        isolated_port() + 1,
        "example_token_verifier",
        std::env::var("NULANG_TOKEN_REVOCATION_READER_PASSWORD").unwrap(),
    )
    .unwrap();
    assert_eq!(
        disconnected.is_revoked("issuer-a", "alice", "unknown"),
        None
    );
}

#[test]
fn reader_role_cannot_write_or_delete_revocations() {
    let mut config = postgres::Config::new();
    config
        .host("127.0.0.1")
        .port(isolated_port())
        .user("example_token_verifier")
        .password(std::env::var("NULANG_TOKEN_REVOCATION_READER_PASSWORD").unwrap())
        .dbname("postgres");
    let mut connection = config.connect(postgres::NoTls).unwrap();
    assert!(connection.execute(
        "INSERT INTO tenant_example_private.revoked_tokens(issuer,subject,jti) VALUES ('issuer-a','alice','forged')", &[]
    ).is_err());
    assert!(connection
        .execute("DELETE FROM tenant_example_private.revoked_tokens", &[])
        .is_err());
}

#[test]
fn unsafe_connection_targets_are_rejected_before_sql() {
    for (host, port) in [
        ("db.internal", 15433),
        ("127.0.0.1", 5432),
        ("localhost", 15433),
    ] {
        assert!(DisposablePostgresRevocation::new(
            host,
            port,
            "example_token_verifier",
            "not-a-real-password".into(),
        )
        .is_none());
    }
}

#[test]
fn live_loopback_http_denies_after_durable_token_revocation() {
    use nulang::runtime::WebDevServer;
    use nulang::web::tenant_pg_reference::{TenantPostgresAuthority, TenantRoleBinding};
    use std::collections::HashMap;
    use std::io::{Read, Write};
    use std::net::{Shutdown, TcpStream};
    use std::time::Duration;

    let membership = HashMap::from([
        (
            "issuer-a|alice".to_string(),
            TenantRoleBinding::new(
                "tenant-a",
                "example_tenant_a",
                std::env::var("NULANG_TENANT_TEST_PASSWORD_A").unwrap(),
            ),
        ),
        (
            "issuer-a|bob".to_string(),
            TenantRoleBinding::new(
                "tenant-b",
                "example_tenant_b",
                std::env::var("NULANG_TENANT_TEST_PASSWORD_B").unwrap(),
            ),
        ),
    ]);
    let authority =
        TenantPostgresAuthority::new("127.0.0.1", isolated_port(), verifier(), membership).unwrap();
    let listener = WebDevServer::bind_disposable_tenant_authority(authority).unwrap();
    let alice = signed("alice", "live-http-revoked");
    let bob = signed("bob", "live-http-bob");

    let get = |token: &str| {
        let mut conn = TcpStream::connect(("127.0.0.1", listener.port)).unwrap();
        conn.set_read_timeout(Some(Duration::from_secs(5))).unwrap();
        conn.write_all(format!(
            "GET /__nulang_test/tenant-faces HTTP/1.1\r\nHost: 127.0.0.1:{}\r\nAuthorization: Bearer {}\r\nConnection: close\r\n\r\n",
            listener.port, token,
        ).as_bytes()).unwrap();
        conn.shutdown(Shutdown::Write).unwrap();
        let mut bytes = Vec::new();
        conn.read_to_end(&mut bytes).unwrap();
        String::from_utf8(bytes).unwrap()
    };
    let before = get(&alice);
    assert!(before.starts_with("HTTP/1.1 200 OK"), "{before}");
    assert!(before.contains("Face A") && !before.contains("Face B"));

    privileged_sql(
        "INSERT INTO tenant_example_private.revoked_tokens(issuer,subject,jti) VALUES ('issuer-a','alice','live-http-revoked')"
    );
    let after = get(&alice);
    assert!(after.starts_with("HTTP/1.1 401 Unauthorized"), "{after}");
    let other = get(&bob);
    assert!(other.starts_with("HTTP/1.1 200 OK"), "{other}");
    assert!(other.contains("Face B") && !other.contains("Face A"));

    privileged_sql(
        "REVOKE SELECT ON tenant_example_private.revoked_tokens FROM example_token_verifier",
    );
    let disconnected = get(&bob);
    assert!(
        disconnected.starts_with("HTTP/1.1 401 Unauthorized"),
        "{disconnected}"
    );
    privileged_sql(
        "GRANT SELECT ON tenant_example_private.revoked_tokens TO example_token_verifier",
    );
    let restored = get(&bob);
    assert!(restored.starts_with("HTTP/1.1 200 OK"), "{restored}");
}

#[test]
fn staged_pinned_key_rotation_respects_persisted_postgres_revocation() {
    use nulang::web::tenant_jwt_reference::PinnedEd25519KeyRing;

    let old = signed("alice", "rotate-old-jti");
    let new_signer = SigningKey::from_bytes(&[72u8; 32]); // test fixture only
    let t = now_for_rotation_test();
    let header = URL_SAFE_NO_PAD.encode(
        serde_json::to_vec(&json!({
            "alg":"EdDSA","kid":"new-key","typ":"at+jwt"
        }))
        .unwrap(),
    );
    let claims = URL_SAFE_NO_PAD.encode(
        serde_json::to_vec(&json!({
            "iss":"issuer-a","sub":"alice","aud":"nulang-tenant-api",
            "exp":t+120,"iat":t-2,"nbf":t-2,"jti":"rotate-new-jti"
        }))
        .unwrap(),
    );
    let input = format!("{header}.{claims}");
    let newer = format!(
        "{input}.{}",
        URL_SAFE_NO_PAD.encode(new_signer.sign(input.as_bytes()).to_bytes())
    );

    let build = |include_old| {
        let mut keys = vec![("new-key".into(), new_signer.verifying_key())];
        if include_old {
            keys.push(("revocation-test-key".into(), signer().verifying_key()));
        }
        PinnedEd25519KeyRing::new("issuer-a", "nulang-tenant-api", keys, revocations()).unwrap()
    };
    let overlapping = build(true);
    assert_eq!(overlapping.verify(&old), Some("issuer-a|alice".into()));
    assert_eq!(overlapping.verify(&newer), Some("issuer-a|alice".into()));
    privileged_sql("INSERT INTO tenant_example_private.revoked_tokens(issuer,subject,jti) VALUES ('issuer-a','alice','rotate-old-jti')");
    assert_eq!(overlapping.verify(&old), None);
    assert_eq!(overlapping.verify(&newer), Some("issuer-a|alice".into()));
    let retired = build(false);
    assert_eq!(retired.verify(&old), None);
    assert_eq!(retired.verify(&newer), Some("issuer-a|alice".into()));
    privileged_sql("INSERT INTO tenant_example_private.revoked_tokens(issuer,subject,jti) VALUES ('issuer-a','alice','rotate-new-jti')");
    assert_eq!(retired.verify(&newer), None);
}

fn now_for_rotation_test() -> i64 {
    i64::try_from(
        SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .unwrap()
            .as_secs(),
    )
    .unwrap()
}
