use nulang_ai_mcp::forge::{ForgeGrant, ForgeOperation, ForgeSession, RepositoryRef};
use nulang_security::{Principal, PrincipalKind, UnixSeconds};

#[test]
fn forge_session_preserves_typed_shared_principal() {
    let principal = Principal::new(PrincipalKind::Agent, "agent:coder-7").unwrap();
    let session = ForgeSession::new(principal.clone(), Vec::new());

    assert_eq!(session.subject, principal);
}

#[test]
fn forge_grant_expiry_uses_explicit_unix_seconds() {
    let repository = RepositoryRef::new("nulang-org", "nulang").unwrap();
    let grant = ForgeGrant::new(repository, [ForgeOperation::RepoRead])
        .with_expiry(UnixSeconds::from_secs(1_700_000_000));

    assert_eq!(
        grant.expires_at,
        Some(UnixSeconds::from_secs(1_700_000_000))
    );
}
