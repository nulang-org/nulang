use nulang::authority::{AuthorityGrant, AuthorityManifest};
use nulang::authorization::{
    AuthorizationContext, AuthorityDelegation, DecisionReason, DelegationConstraints,
    DelegationError, DelegationId, DelegationProvenance, Principal, PrincipalKind,
    RevocationDomainId, RevocationEpoch, RevocationVersion, UnixSeconds,
};

fn principal(kind: PrincipalKind, id: &str) -> Principal {
    Principal::new(kind, id).unwrap()
}

fn t(seconds: u64) -> UnixSeconds {
    UnixSeconds::from_secs(seconds)
}

fn revocation(domain: &str, epoch: u64) -> RevocationVersion {
    RevocationVersion::new(
        RevocationDomainId::new(domain).unwrap(),
        RevocationEpoch::new(epoch),
    )
}

fn provenance(
    id: &str,
    parent_id: Option<&str>,
    issued_at: u64,
    revocation: RevocationVersion,
) -> DelegationProvenance {
    DelegationProvenance::new(
        DelegationId::new(id).unwrap(),
        parent_id.map(|value| DelegationId::new(value).unwrap()),
        t(issued_at),
        revocation,
    )
}

fn context(now: u64, revocation: RevocationVersion) -> AuthorizationContext {
    AuthorizationContext::new(t(now), revocation)
}

fn manifest(token: &str) -> AuthorityManifest {
    AuthorityManifest::from_tokens([token]).unwrap()
}

#[test]
fn root_delegation_preserves_immutable_provenance() {
    let delegation = AuthorityDelegation::issue(
        provenance(
            "delegation:root",
            None,
            100,
            revocation("tenant:acme/agent-authority", 7),
        ),
        principal(PrincipalKind::Human, "human:admin"),
        principal(PrincipalKind::Agent, "agent:planner"),
        manifest("Env::Read(API_URL)"),
        DelegationConstraints::new(Some(t(100)), Some(t(500)), true).unwrap(),
    )
    .unwrap();

    assert_eq!(delegation.provenance().id().as_str(), "delegation:root");
    assert_eq!(delegation.provenance().parent_id(), None);
    assert_eq!(delegation.provenance().issued_at(), t(100));
}

#[test]
fn authorization_denies_delegation_revoked_by_a_newer_epoch() {
    let agent = principal(PrincipalKind::Agent, "agent:planner");
    let delegation = AuthorityDelegation::issue(
        provenance(
            "delegation:root",
            None,
            100,
            revocation("tenant:acme/agent-authority", 7),
        ),
        principal(PrincipalKind::Human, "human:admin"),
        agent.clone(),
        manifest("Env::Read(API_URL)"),
        DelegationConstraints::new(Some(t(100)), Some(t(500)), false).unwrap(),
    )
    .unwrap();
    let grant: AuthorityGrant = "Env::Read(API_URL)".parse().unwrap();

    let decision = delegation.authorize(
        &agent,
        &grant,
        &context(200, revocation("tenant:acme/agent-authority", 8)),
    );

    assert_eq!(
        decision.reason(),
        Some(&DecisionReason::Revoked {
            delegation_epoch: RevocationEpoch::new(7),
            current_epoch: RevocationEpoch::new(8),
        })
    );
}

#[test]
fn authorization_fails_closed_on_revocation_domain_mismatch() {
    let agent = principal(PrincipalKind::Agent, "agent:planner");
    let delegation = AuthorityDelegation::issue(
        provenance(
            "delegation:root",
            None,
            100,
            revocation("tenant:acme/agent-authority", 7),
        ),
        principal(PrincipalKind::Human, "human:admin"),
        agent.clone(),
        manifest("Env::Read(API_URL)"),
        DelegationConstraints::default(),
    )
    .unwrap();
    let grant: AuthorityGrant = "Env::Read(API_URL)".parse().unwrap();

    let decision = delegation.authorize(
        &agent,
        &grant,
        &context(200, revocation("tenant:other/agent-authority", 7)),
    );

    assert!(matches!(
        decision.reason(),
        Some(DecisionReason::RevocationDomainMismatch { .. })
    ));
}

#[test]
fn authorization_fails_closed_when_policy_view_is_older_than_delegation() {
    let agent = principal(PrincipalKind::Agent, "agent:planner");
    let delegation = AuthorityDelegation::issue(
        provenance(
            "delegation:root",
            None,
            100,
            revocation("tenant:acme/agent-authority", 7),
        ),
        principal(PrincipalKind::Human, "human:admin"),
        agent.clone(),
        manifest("Env::Read(API_URL)"),
        DelegationConstraints::default(),
    )
    .unwrap();
    let grant: AuthorityGrant = "Env::Read(API_URL)".parse().unwrap();

    let decision = delegation.authorize(
        &agent,
        &grant,
        &context(200, revocation("tenant:acme/agent-authority", 6)),
    );

    assert_eq!(
        decision.reason(),
        Some(&DecisionReason::PolicyVersionBehind {
            delegation_epoch: RevocationEpoch::new(7),
            current_epoch: RevocationEpoch::new(6),
        })
    );
}

#[test]
fn child_delegation_records_parent_issue_time_and_current_revocation_version() {
    let domain = "tenant:acme/agent-authority";
    let parent = AuthorityDelegation::issue(
        provenance("delegation:root", None, 100, revocation(domain, 7)),
        principal(PrincipalKind::Human, "human:admin"),
        principal(PrincipalKind::Agent, "agent:planner"),
        manifest("Env::Read(API_URL)"),
        DelegationConstraints::new(Some(t(100)), Some(t(500)), true).unwrap(),
    )
    .unwrap();

    let child = parent
        .delegate(
            &context(200, revocation(domain, 7)),
            DelegationId::new("delegation:child").unwrap(),
            principal(PrincipalKind::Agent, "agent:worker"),
            manifest("Env::Read(API_URL)"),
            DelegationConstraints::new(Some(t(200)), Some(t(400)), false).unwrap(),
        )
        .unwrap();

    assert_eq!(child.provenance().id().as_str(), "delegation:child");
    assert_eq!(
        child.provenance().parent_id().map(DelegationId::as_str),
        Some("delegation:root")
    );
    assert_eq!(child.provenance().issued_at(), t(200));
    assert_eq!(
        child.provenance().revocation(),
        &revocation(domain, 7)
    );
}

#[test]
fn revoked_parent_cannot_redelegate() {
    let domain = "tenant:acme/agent-authority";
    let parent = AuthorityDelegation::issue(
        provenance("delegation:root", None, 100, revocation(domain, 7)),
        principal(PrincipalKind::Human, "human:admin"),
        principal(PrincipalKind::Agent, "agent:planner"),
        manifest("Env::Read(API_URL)"),
        DelegationConstraints::new(None, None, true).unwrap(),
    )
    .unwrap();

    let error = parent
        .delegate(
            &context(200, revocation(domain, 8)),
            DelegationId::new("delegation:child").unwrap(),
            principal(PrincipalKind::Agent, "agent:worker"),
            manifest("Env::Read(API_URL)"),
            DelegationConstraints::default(),
        )
        .unwrap_err();

    assert!(matches!(
        error,
        DelegationError::InactiveParent(DecisionReason::Revoked { .. })
    ));
}

#[test]
fn root_issue_rejects_preexisting_parent_and_child_rejects_direct_id_reuse() {
    let domain = "tenant:acme/agent-authority";
    let bad_root = AuthorityDelegation::issue(
        provenance(
            "delegation:root",
            Some("delegation:unexpected-parent"),
            100,
            revocation(domain, 7),
        ),
        principal(PrincipalKind::Human, "human:admin"),
        principal(PrincipalKind::Agent, "agent:planner"),
        manifest("Env::Read(API_URL)"),
        DelegationConstraints::default(),
    );
    assert!(matches!(bad_root, Err(DelegationError::RootHasParent(_))));

    let parent = AuthorityDelegation::issue(
        provenance("delegation:root", None, 100, revocation(domain, 7)),
        principal(PrincipalKind::Human, "human:admin"),
        principal(PrincipalKind::Agent, "agent:planner"),
        manifest("Env::Read(API_URL)"),
        DelegationConstraints::new(None, None, true).unwrap(),
    )
    .unwrap();

    let error = parent
        .delegate(
            &context(200, revocation(domain, 7)),
            DelegationId::new("delegation:root").unwrap(),
            principal(PrincipalKind::Agent, "agent:worker"),
            manifest("Env::Read(API_URL)"),
            DelegationConstraints::default(),
        )
        .unwrap_err();
    assert!(matches!(error, DelegationError::DelegationIdReuse(_)));
}
