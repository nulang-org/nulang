use nulang::authority::{AuthorityGrant, AuthorityManifest};
use nulang::authorization::{
    AuthorityDelegation, ConstraintField, DecisionReason, DelegationConstraints, DelegationError,
    Principal, PrincipalKind, UnixSeconds,
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
        issuer,
        agent.clone(),
        manifest(&["Secret::Read(STRIPE_KEY)"]),
        DelegationConstraints::new(Some(t(1_000)), Some(t(2_000)), false).unwrap(),
    );
    let requested: AuthorityGrant = "Secret::Read(STRIPE_KEY)".parse().unwrap();

    let decision = delegation.authorize(&agent, &requested, t(1_500));

    assert!(decision.is_allowed());
    assert_eq!(decision.reason(), None);
}

#[test]
fn authorization_denies_wrong_principal_missing_grant_and_expired_delegation() {
    let issuer = principal(PrincipalKind::Human, "human:david");
    let agent = principal(PrincipalKind::Agent, "agent:invoice");
    let other_agent = principal(PrincipalKind::Agent, "agent:reporting");
    let delegation = AuthorityDelegation::issue(
        issuer,
        agent.clone(),
        manifest(&["Secret::Read(STRIPE_KEY)"]),
        DelegationConstraints::new(Some(t(1_000)), Some(t(2_000)), false).unwrap(),
    );
    let allowed: AuthorityGrant = "Secret::Read(STRIPE_KEY)".parse().unwrap();
    let missing: AuthorityGrant = "Secret::Read(OTHER_KEY)".parse().unwrap();

    assert!(matches!(
        delegation
            .authorize(&other_agent, &allowed, t(1_500))
            .reason(),
        Some(DecisionReason::PrincipalMismatch { .. })
    ));
    assert_eq!(
        delegation.authorize(&agent, &missing, t(1_500)).reason(),
        Some(&DecisionReason::MissingGrant(missing))
    );
    assert!(matches!(
        delegation.authorize(&agent, &allowed, t(2_000)).reason(),
        Some(DecisionReason::Expired { .. })
    ));
}

#[test]
fn delegation_attenuates_authority_and_sets_the_current_holder_as_issuer() {
    let human = principal(PrincipalKind::Human, "human:david");
    let parent_agent = principal(PrincipalKind::Agent, "agent:planner");
    let child_agent = principal(PrincipalKind::Agent, "agent:browser");
    let parent = AuthorityDelegation::issue(
        human,
        parent_agent.clone(),
        manifest(&["Net::TcpOut(api.example.com:443)", "Secret::Read(API_KEY)"]),
        DelegationConstraints::new(Some(t(1_000)), Some(t(5_000)), true).unwrap(),
    );

    let child = parent
        .delegate(
            t(1_500),
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
        human,
        parent_agent,
        manifest(&["Net::TcpOut(api.example.com:443)"]),
        DelegationConstraints::new(None, None, true).unwrap(),
    );
    let escalated: AuthorityGrant = "Secret::Read(API_KEY)".parse().unwrap();

    assert_eq!(
        parent.delegate(
            t(1_500),
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
        human,
        parent_agent,
        manifest(&["Net::TcpOut(api.example.com:443)"]),
        DelegationConstraints::new(Some(t(1_000)), Some(t(5_000)), true).unwrap(),
    );

    assert_eq!(
        parent.delegate(
            t(1_500),
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
        human,
        parent_agent,
        manifest(&["Net::TcpOut(api.example.com:443)"]),
        DelegationConstraints::new(None, Some(t(5_000)), false).unwrap(),
    );

    assert_eq!(
        parent.delegate(
            t(1_500),
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
