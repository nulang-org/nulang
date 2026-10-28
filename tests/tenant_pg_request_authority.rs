//! Real PostgreSQL admission proof. Run ONLY through the guarded disposable
//! wrapper tests/run_tenant_pg_authority_ci.sh.
#![cfg(feature = "postgres")]

use nulang::runtime::{HttpMethod, HttpRequest};
use nulang::web::tenant_pg_reference::{
    TenantAuthorityError, TenantPostgresAuthority, TenantRoleBinding, TrustedTokenVerifier,
};
use postgres::Transaction;
use std::collections::HashMap;

#[derive(Clone)]
struct TestVerifier {
    /// This is an intentionally fake server-side verifier for contract tests,
    /// NOT a cryptographic production identity provider.
    issued: HashMap<String, String>,
}

impl TrustedTokenVerifier for TestVerifier {
    fn verify(&self, bearer: &str) -> Option<String> {
        self.issued.get(bearer).cloned()
    }
}

fn request(token: Option<&str>, forged_tenant: Option<&str>) -> HttpRequest {
    let mut headers = vec![("Host".to_string(), "localhost".to_string())];
    if let Some(token) = token {
        headers.push(("Authorization".to_string(), format!("Bearer {token}")));
    }
    if let Some(tenant) = forged_tenant {
        headers.push(("X-Tenant-Id".to_string(), tenant.to_string()));
    }
    HttpRequest {
        method: HttpMethod::Get,
        path: "/inventory?tenant_id=tenant-b".to_string(),
        headers,
        body: Vec::new(),
    }
}

fn authority_with_binding(
    tenant_a_role: &str,
    tenant_a_password: String,
) -> TenantPostgresAuthority<TestVerifier> {
    assert_eq!(
        std::env::var("NULANG_TEST_POSTGRES_DISPOSABLE").as_deref(),
        Ok("1"),
        "never use a non-disposable PostgreSQL connection"
    );
    let port = std::env::var("PGPORT").unwrap().parse::<u16>().unwrap();
    let members = HashMap::from([
        (
            "issuer-a|alice".to_string(),
            TenantRoleBinding::new("tenant-a", tenant_a_role, tenant_a_password),
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
    let verifier = TestVerifier {
        issued: HashMap::from([
            (
                "test-opaque-alice".to_string(),
                "issuer-a|alice".to_string(),
            ),
            ("test-opaque-bob".to_string(), "issuer-a|bob".to_string()),
            (
                "test-no-membership".to_string(),
                "issuer-a|removed".to_string(),
            ),
        ]),
    };
    TenantPostgresAuthority::new("127.0.0.1", port, verifier, members).unwrap()
}

fn authority() -> TenantPostgresAuthority<TestVerifier> {
    authority_with_binding(
        "example_tenant_a",
        std::env::var("NULANG_TENANT_TEST_PASSWORD_A").unwrap(),
    )
}

fn face_names(tx: &mut Transaction<'_>) -> Result<Vec<String>, postgres::Error> {
    Ok(tx
        .query(
            "SELECT name FROM tenant_example.billboard_faces ORDER BY id",
            &[],
        )?
        .into_iter()
        .map(|row| row.get(0))
        .collect())
}

#[test]
fn verified_identity_selects_database_login_not_client_tenant() {
    let authority = authority();
    let a = authority
        .execute(
            &request(Some("test-opaque-alice"), Some("tenant-b")),
            face_names,
        )
        .expect("Alice must see only tenant A");
    assert_eq!(a, vec!["New face", "Face A"]);
    let b = authority
        .execute(
            &request(Some("test-opaque-bob"), Some("tenant-a")),
            face_names,
        )
        .expect("Bob must see only tenant B");
    assert_eq!(b, vec!["Foreign Face", "Face B"]);
    assert_eq!(
        authority
            .execute(
                &request(Some("test-opaque-alice"), Some("tenant-b")),
                face_names
            )
            .unwrap(),
        a,
        "a subsequent request must not inherit B's tenant connection"
    );
}

#[test]
fn missing_forged_duplicate_or_unmapped_identity_fails_before_sql() {
    let authority = authority();
    for request in [
        request(None, Some("tenant-a")),
        request(Some("wrong-token"), Some("tenant-a")),
        request(Some("test-no-membership"), Some("tenant-a")),
        request(Some("test-opaque-alice\r\n"), None),
    ] {
        let result = authority.execute(&request, |_tx| -> Result<(), postgres::Error> {
            panic!("unauthorized request reached SQL");
        });
        assert!(matches!(result, Err(TenantAuthorityError::Unauthorized)));
    }
    let mut duplicate = request(Some("test-opaque-alice"), None);
    duplicate
        .headers
        .push(("authorization".into(), "Bearer test-opaque-bob".into()));
    assert!(matches!(
        authority.execute(&duplicate, |_tx| -> Result<(), postgres::Error> {
            panic!("duplicate Authorization reached SQL");
        }),
        Err(TenantAuthorityError::Unauthorized)
    ));
}

#[test]
fn mismatched_server_role_and_tenant_mapping_fails_closed() {
    let authority = authority_with_binding(
        "example_tenant_b",
        std::env::var("NULANG_TENANT_TEST_PASSWORD_B").unwrap(),
    );
    assert!(matches!(
        authority.execute(
            &request(Some("test-opaque-alice"), None),
            |_tx| -> Result<(), postgres::Error> {
                panic!("mismatched PostgreSQL identity reached SQL");
            }
        ),
        Err(TenantAuthorityError::RoleMismatch)
    ));
}

#[test]
fn denied_foreign_write_and_failed_transaction_never_commit() {
    let authority = authority();
    let alice = request(Some("test-opaque-alice"), None);
    let before = authority.execute(&alice, face_names).unwrap();
    let denied = authority.execute(&alice, |tx| {
        tx.execute(
            "INSERT INTO tenant_example.billboard_faces(tenant_id,id,name) VALUES ('tenant-b','forged-rust','Evil')",
            &[],
        )?;
        Ok(())
    });
    assert!(matches!(denied, Err(TenantAuthorityError::Database)));
    assert_eq!(authority.execute(&alice, face_names).unwrap(), before);

    let rollback = authority.execute(&alice, |tx| {
        tx.execute(
            "UPDATE tenant_example.billboard_faces SET name='Partial' WHERE tenant_id='tenant-a' AND id='shared'",
            &[],
        )?;
        tx.execute(
            "INSERT INTO tenant_example.campaigns(tenant_id,id,face_id,title) VALUES ('tenant-a','bad-fk','only-b','Bad')",
            &[],
        )?;
        Ok(())
    });
    assert!(matches!(rollback, Err(TenantAuthorityError::Database)));
    assert_eq!(authority.execute(&alice, face_names).unwrap(), before);
}
