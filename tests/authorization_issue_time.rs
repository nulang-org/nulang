use nulang::authority::{AuthorityGrant, AuthorityManifest};
use nulang::authorization::{
    AuthorizationContext, AuthorityDelegation, DecisionReason, DelegationConstraints,
    DelegationId, DelegationProvenance, Principal, PrincipalKind, RevocationDomainId,
    RevocationEpoch, RevocationVersion, UnixSeconds,
};

#[test]
fn delegation_cannot_authorize_before_its_issue_time() {
    let domain = RevocationDomainId::new("tenant:acme/agent-authority").unwrap();
    let revocation = RevocationVersion::new(domain, RevocationEpoch::new(7));
    let agent = Principal::new(PrincipalKind::Agent, "agent:planner").unwrap();
    let delegation = AuthorityDelegation::issue(
        DelegationProvenance::new(
            DelegationId::new("delegation:root").unwrap(),
            None,
            UnixSeconds::from_secs(100),
            revocation.clone(),
        ),
        Principal::new(PrincipalKind::Human, "human:admin").unwrap(),
        agent.clone(),
        AuthorityManifest::from_tokens(["Env::Read(API_URL)"]).unwrap(),
        DelegationConstraints::default(),
    )
    .unwrap();
    let grant: AuthorityGrant = "Env::Read(API_URL)".parse().unwrap();
    let context = AuthorizationContext::new(UnixSeconds::from_secs(99), revocation);

    let decision = delegation.authorize(&agent, &grant, &context);

    assert_eq!(
        decision.reason(),
        Some(&DecisionReason::NotYetIssued {
            issued_at: UnixSeconds::from_secs(100),
            now: UnixSeconds::from_secs(99),
        })
    );
}
