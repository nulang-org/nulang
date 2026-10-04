use nulang_security::{
    DelegationId, DelegationProvenance, Principal, PrincipalKind, RevocationDomainId,
    RevocationEpoch, RevocationVersion, UnixSeconds,
};

#[test]
fn principal_identity_is_typed_serializable_and_non_empty() {
    let workload = Principal::new(PrincipalKind::Workload, "workload:billing/api").unwrap();

    assert_eq!(workload.kind(), PrincipalKind::Workload);
    assert_eq!(workload.id(), "workload:billing/api");
    assert!(Principal::new(PrincipalKind::Agent, "   ").is_err());
}

#[test]
fn unix_seconds_has_an_explicit_unit() {
    let instant = UnixSeconds::from_secs(1_700_000_000);

    assert_eq!(instant.as_secs(), 1_700_000_000);
    assert_eq!(instant.to_string(), "1700000000");
}

#[test]
fn delegation_provenance_tracks_parent_issue_time_and_scoped_revocation_version() {
    let root = DelegationId::new("delegation:root").unwrap();
    let child = DelegationId::new("delegation:child").unwrap();
    let domain = RevocationDomainId::new("tenant:acme/agent-authority").unwrap();
    let revocation = RevocationVersion::new(domain.clone(), RevocationEpoch::new(7));
    let provenance = DelegationProvenance::new(
        child.clone(),
        Some(root.clone()),
        UnixSeconds::from_secs(1_700_000_000),
        revocation.clone(),
    );

    assert_eq!(provenance.id(), &child);
    assert_eq!(provenance.parent_id(), Some(&root));
    assert_eq!(provenance.issued_at(), UnixSeconds::from_secs(1_700_000_000));
    assert_eq!(provenance.revocation(), &revocation);
    assert_eq!(provenance.revocation().domain(), &domain);
    assert_eq!(provenance.revocation().epoch(), RevocationEpoch::new(7));
}

#[test]
fn delegation_and_revocation_domain_ids_reject_empty_values() {
    assert!(DelegationId::new("").is_err());
    assert!(DelegationId::new("   ").is_err());
    assert!(RevocationDomainId::new("").is_err());
    assert!(RevocationDomainId::new("   ").is_err());
}

#[test]
fn revocation_versions_compare_epochs_only_within_the_same_domain() {
    let domain = RevocationDomainId::new("tenant:acme/agent-authority").unwrap();
    let other = RevocationDomainId::new("tenant:other/agent-authority").unwrap();
    let issued = RevocationVersion::new(domain.clone(), RevocationEpoch::new(7));
    let current = RevocationVersion::new(domain, RevocationEpoch::new(8));
    let unrelated = RevocationVersion::new(other, RevocationEpoch::new(99));

    assert_eq!(issued.compare_epoch(&current), Some(std::cmp::Ordering::Less));
    assert_eq!(issued.compare_epoch(&unrelated), None);
}
