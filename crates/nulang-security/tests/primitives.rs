use nulang_security::{
    DelegationId, DelegationProvenance, Principal, PrincipalKind, RevocationEpoch, UnixSeconds,
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
fn delegation_provenance_tracks_parent_issue_time_and_revocation_epoch() {
    let root = DelegationId::new("delegation:root").unwrap();
    let child = DelegationId::new("delegation:child").unwrap();
    let provenance = DelegationProvenance::new(
        child.clone(),
        Some(root.clone()),
        UnixSeconds::from_secs(1_700_000_000),
        RevocationEpoch::new(7),
    );

    assert_eq!(provenance.id(), &child);
    assert_eq!(provenance.parent_id(), Some(&root));
    assert_eq!(provenance.issued_at(), UnixSeconds::from_secs(1_700_000_000));
    assert_eq!(provenance.revocation_epoch(), RevocationEpoch::new(7));
}

#[test]
fn delegation_ids_reject_empty_values() {
    assert!(DelegationId::new("").is_err());
    assert!(DelegationId::new("   ").is_err());
}
