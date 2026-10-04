use nulang_security::{
    DelegationId, Principal, PrincipalKind, RevocationDomainId, RevocationEpoch, RevocationVersion,
    UnixSeconds,
};

#[test]
fn principal_kind_uses_stable_snake_case_json_values() {
    assert_eq!(serde_json::to_string(&PrincipalKind::Workload).unwrap(), "\"workload\"");
}

#[test]
fn principal_round_trips_without_losing_kind_or_id() {
    let principal = Principal::new(PrincipalKind::Agent, "agent:planner").unwrap();
    let encoded = serde_json::to_string(&principal).unwrap();
    let decoded: Principal = serde_json::from_str(&encoded).unwrap();

    assert_eq!(decoded, principal);
}

#[test]
fn scalar_security_types_round_trip_as_their_wire_scalars() {
    let id = DelegationId::new("delegation:42").unwrap();
    let domain = RevocationDomainId::new("tenant:acme/agent-authority").unwrap();
    assert_eq!(serde_json::to_string(&id).unwrap(), "\"delegation:42\"");
    assert_eq!(serde_json::to_string(&domain).unwrap(), "\"tenant:acme/agent-authority\"");
    assert_eq!(serde_json::to_string(&UnixSeconds::from_secs(42)).unwrap(), "42");
    assert_eq!(serde_json::to_string(&RevocationEpoch::new(9)).unwrap(), "9");
}

#[test]
fn revocation_version_wire_form_carries_both_domain_and_epoch() {
    let version = RevocationVersion::new(
        RevocationDomainId::new("tenant:acme/agent-authority").unwrap(),
        RevocationEpoch::new(9),
    );
    let value = serde_json::to_value(&version).unwrap();

    assert_eq!(value["domain"], "tenant:acme/agent-authority");
    assert_eq!(value["epoch"], 9);
}
