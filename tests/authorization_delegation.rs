use nulang::authority::{AuthorityGrant, AuthorityManifest};
use nulang::authorization::{
    AuthorizationContext, AuthorityDelegation, ConstraintField, DecisionReason,
    DelegationConstraints, DelegationError, DelegationId, DelegationProvenance, Principal,
    PrincipalKind, RevocationDomainId, RevocationEpoch, RevocationVersion, UnixSeconds,
};

fn manifest(tokens: &[&str]) -> AuthorityManifest {
    AuthorityManifest::from_tokens(tokens.iter().copied()).expect("valid authority fixture")
}

fn principal(kind: PrincipalKind, id: &str) -> Principal {
    Principal::new(kind, id).expect("valid principal fixture")
}

fn t(unix_secs: u64) -> UnixSeconds {
    UnixSeconds::from_secs(unix_secs)
}

fn revocation() -> RevocationVersion {
    RevocationVersion::new(
        RevocationDomainId::new("test:delegation").unwrap(),
        RevocationEpoch::new(1),
    )
}

fn provenance(id: &str, issued_at: u64) -> DelegationProvenance {
    DelegationProvenance::new(
        DelegationId::new(id).unwrap(),
        None,
        t(issued_at),
        revocation(),
    )
}

fn context(now: u64) -> AuthorizationContext {
    AuthorizationContext::new(t(now), revocation())
}

#[test]
fn all_principal_classes_include_explicit_workloads() {
    let workload = principal(PrincipalKind::Workload, "workload:payments-api");
    assert_eq!(workload.kind(), PrincipalKind::Workload);
}

#[test]
fn exact_grant_is_allowed_for_the_delegated_principal() {
    let issuer = principal(PrincipalKind::Human, "human:david");
    let agent = principal(PrincipalKind::Agent, "agent:invoice");
    let delegation = AuthorityDelegation::issue(
        provenance("delegation:exact", 900),
        issuer,
        agent.clone(),
        manifest(&["Secret::Read(STRIPE_KEY)"]),
        DelegationConstraints::new(Some(t(1_000)), Some(t(2_000)), false).unwrap(),
    )
    .unwrap();
    let requested: AuthorityGrant = "Secret::Read(STRIPE_KEY)".parse().unwrap();

    let decision = delegation.authorize(&agent, &requested, &context(1_500));

    assert!(decision.is_allowed());
    assert_eq!(decision.reason(), None);
}

#[test]
fn authorization_denies_wrong_principal_missing_grant_and_expired_delegation() {
    let issuer = principal(PrincipalKind::Human, "human:david");
    let agent = principal(PrincipalKind::Agent, "agent:invoice");
    let other_agent = principal(PrincipalKind::Agent, "agent:reporting");
    let delegation = AuthorityDelegation::issue(
        provenance("delegation:deny-cases", 900),
        issuer,
        agent.clone(),
        manifest(&["Secret::Read(STRIPE_KEY)"]),
        DelegationConstraints::new(Some(t(1_000)), Some(t(2_000)), false).unwrap(),
    )
    .unwrap();
    let allowed: AuthorityGrant = "Secret::Read(STRIPE_KEY)".parse().unwrap();
    let missing: AuthorityGrant = "Secret::Read(OTHER_KEY)".parse().unwrap();

    assert!(matches!(
        delegation
            .authorize(&other_agent, &allowed, &context(1_500))
            .reason(),
        Some(DecisionReason::PrincipalMismatch { .. })
    ));
    assert_eq!(
        delegation
            .authorize(&agent, &missing, &context(1_500))
            .reason(),
        Some(&DecisionReason::MissingGrant(missing))
    );
    assert!(matches!(
        delegation
            .authorize(&agent, &allowed, &context(2_000))
            .reason(),
        Some(DecisionReason::Expired { .. })
    ));
}

#[test]
fn delegation_attenuates_authority_and_sets_the_current_holder_as_issuer() {
    let human = principal(PrincipalKind::Human, "human:david");
    let parent_agent = principal(PrincipalKind::Agent, "agent:planner");
    let child_agent = principal(PrincipalKind::Agent, "agent:browser");
    let parent = AuthorityDelegation::issue(
        provenance("delegation:parent", 900),
        human,
        parent_agent.clone(),
        manifest(&[
            "Net::TcpOut(api.example.com:443)",
            "Secret::Read(API_KEY)",
        ]),
        DelegationConstraints::new(Some(t(1_000)), Some(t(5_000)), true).unwrap(),
    )
    .unwrap();

    let child = parent
        .delegate(
            &context(1_500),
            DelegationId::new("delegation:child").unwrap(),
            child_agent.clone(),
            manifest(&["Net::TcpOut(api.example.com:443)"]),
            DelegationConstraints::new(Some(t(1_500)), Some(t(3_000)), false).unwrap(),
        )
        .unwrap();

    assert_eq!(child.issuer(), &parent_agent);
    assert_eq!(child.subject(), &child_agent);
    assert_eq!(child.authority().len(), 1);
    assert!(child.authority().allows_tcp_out("api.example.com", 443));
}

#[test]
fn delegation_rejects_authority_escalation() {
    let human = principal(PrincipalKind::Human, "human:david");
    let parent_agent = principal(PrincipalKind::Agent, "agent:planner");
    let child_agent = principal(PrincipalKind::Agent, "agent:browser");
    let parent = AuthorityDelegation::issue(
        provenance("delegation:parent", 1_000),
        human,
        parent_agent,
        manifest(&["Net::TcpOut(api.example.com:443)"]),
        DelegationConstraints::new(None, None, true).unwrap(),
    )
    .unwrap();
    let escalated: AuthorityGrant = "Secret::Read(API_KEY)".parse().unwrap();

    assert_eq!(
        parent.delegate(
            &context(1_500),
            DelegationId::new("delegation:child").unwrap(),
            child_agent,
            AuthorityManifest::from_grants([escalated.clone()]),
            DelegationConstraints::default(),
        ),
        Err(DelegationError::AuthorityEscalation(escalated))
    );
}

#[test]
fn delegation_rejects_temporal_constraint_expansion() {
    let human = principal(PrincipalKind::Human, "human:david");
    let parent_agent = principal(PrincipalKind::Agent, "agent:planner");
    let child_agent = principal(PrincipalKind::Agent, "agent:browser");
    let parent = AuthorityDelegation::issue(
        provenance("delegation:parent", 900),
        human,
        parent_agent,
        manifest(&["Net::TcpOut(api.example.com:443)"]),
        DelegationConstraints::new(Some(t(1_000)), Some(t(5_000)), true).unwrap(),
    )
    .unwrap();

    assert_eq!(
        parent.delegate(
            &context(1_500),
            DelegationId::new("delegation:child").unwrap(),
            child_agent,
            manifest(&["Net::TcpOut(api.example.com:443)"]),
            DelegationConstraints::new(Some(t(500)), Some(t(4_000)), false).unwrap(),
        ),
        Err(DelegationError::ConstraintExpansion(
            ConstraintField::NotBefore
        ))
    );
}

#[test]
fn non_redelegable_authority_cannot_be_delegated_again() {
    let human = principal(PrincipalKind::Human, "human:david");
    let parent_agent = principal(PrincipalKind::Agent, "agent:planner");
    let child_agent = principal(PrincipalKind::Agent, "agent:browser");
    let parent = AuthorityDelegation::issue(
        provenance("delegation:parent", 1_000),
        human,
        parent_agent,
        manifest(&["Net::TcpOut(api.example.com:443)"]),
        DelegationConstraints::new(None, Some(t(5_000)), false).unwrap(),
    )
    .unwrap();

    assert_eq!(
        parent.delegate(
            &context(1_500),
            DelegationId::new("delegation:child").unwrap(),
            child_agent,
            manifest(&["Net::TcpOut(api.example.com:443)"]),
            DelegationConstraints::new(None, Some(t(3_000)), false).unwrap(),
        ),
        Err(DelegationError::RedelegationForbidden)
    );
}

#[test]
fn invalid_time_window_is_rejected() {
    assert_eq!(
        DelegationConstraints::new(Some(t(2_000)), Some(t(2_000)), false),
        Err(DelegationError::InvalidTimeWindow {
            not_before: t(2_000),
            expires_at: t(2_000),
        })
    );
}

#[test]
fn unix_time_unit_is_explicit_at_the_api_boundary() {
    let instant = UnixSeconds::from_secs(1_791_116_285);
    assert_eq!(instant.as_secs(), 1_791_116_285);
}
