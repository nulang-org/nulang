use nulang::authority::AuthorityGrant;
use nulang::authorization::{
    AuthorityDelegation, DelegationConstraints, Principal, PrincipalKind, UnixSeconds,
};
use nulang::authzen::{
    AccessEvaluationRequest, AuthzenAction, AuthzenContext, AuthzenError, AuthzenResource,
    AuthzenSubject, AuthorityGrantMapper, DelegationPdp,
};
use serde_json::{json, Value};
use std::collections::BTreeMap;

fn t(seconds: u64) -> UnixSeconds {
    UnixSeconds::from_secs(seconds)
}

fn principal(kind: PrincipalKind, id: &str) -> Principal {
    Principal::new(kind, id).unwrap()
}

#[derive(Clone, Copy)]
struct SecretMapper;

impl AuthorityGrantMapper for SecretMapper {
    fn required_grant(
        &self,
        request: &AccessEvaluationRequest,
    ) -> Result<AuthorityGrant, AuthzenError> {
        if request.action.name == "read" && request.resource.r#type == "secret" {
            return Ok(AuthorityGrant::SecretRead {
                name: request.resource.id.clone(),
            });
        }
        Err(AuthzenError::UnmappedRequest)
    }
}

fn request(subject: AuthzenSubject, secret: &str) -> AccessEvaluationRequest {
    AccessEvaluationRequest {
        subject,
        action: AuthzenAction {
            name: "read".to_string(),
            properties: None,
        },
        resource: AuthzenResource {
            r#type: "secret".to_string(),
            id: secret.to_string(),
            properties: None,
        },
        context: None,
    }
}

fn pdp() -> DelegationPdp<SecretMapper> {
    let issuer = principal(PrincipalKind::Human, "human:david");
    let agent = principal(PrincipalKind::Agent, "agent:billing");
    let authority = nulang::authority::AuthorityManifest::from_grants([
        AuthorityGrant::SecretRead {
            name: "STRIPE_KEY".to_string(),
        },
    ]);
    let delegation = AuthorityDelegation::issue(
        issuer,
        agent,
        authority,
        DelegationConstraints::new(Some(t(1_000)), Some(t(2_000)), false).unwrap(),
    );
    DelegationPdp::new(delegation, SecretMapper)
}

#[test]
fn principal_round_trips_to_authzen_subject_without_string_conventions() {
    let workload = principal(PrincipalKind::Workload, "workload:payments-api");
    let subject = AuthzenSubject::from(&workload);

    assert_eq!(subject.r#type, "workload");
    assert_eq!(subject.id, "workload:payments-api");
    assert_eq!(Principal::try_from(&subject).unwrap(), workload);
}

#[test]
fn access_evaluation_serializes_as_authzen_sarc_shape() {
    let mut context_values = BTreeMap::new();
    context_values.insert("trace_id".to_string(), json!("trace-123"));
    let evaluation = AccessEvaluationRequest {
        subject: AuthzenSubject {
            r#type: "agent".to_string(),
            id: "agent:billing".to_string(),
            properties: None,
        },
        action: AuthzenAction {
            name: "read".to_string(),
            properties: None,
        },
        resource: AuthzenResource {
            r#type: "secret".to_string(),
            id: "STRIPE_KEY".to_string(),
            properties: None,
        },
        context: Some(AuthzenContext(context_values)),
    };

    let value = serde_json::to_value(evaluation).unwrap();
    assert_eq!(value["subject"]["type"], "agent");
    assert_eq!(value["subject"]["id"], "agent:billing");
    assert_eq!(value["action"]["name"], "read");
    assert_eq!(value["resource"]["type"], "secret");
    assert_eq!(value["resource"]["id"], "STRIPE_KEY");
    assert_eq!(value["context"]["trace_id"], "trace-123");
    assert!(value["subject"].get("properties").is_none());
}

#[test]
fn adapter_allows_mapped_exact_authority() {
    let subject = AuthzenSubject {
        r#type: "agent".to_string(),
        id: "agent:billing".to_string(),
        properties: None,
    };

    let decision = pdp()
        .evaluate(&request(subject, "STRIPE_KEY"), t(1_500))
        .unwrap();

    assert!(decision.decision);
    assert!(decision.context.is_none());
}

#[test]
fn adapter_returns_authzen_deny_with_stable_reason_context() {
    let subject = AuthzenSubject {
        r#type: "agent".to_string(),
        id: "agent:billing".to_string(),
        properties: None,
    };

    let decision = pdp()
        .evaluate(&request(subject, "OTHER_KEY"), t(1_500))
        .unwrap();

    assert!(!decision.decision);
    assert_eq!(
        decision.context.unwrap().0.get("reason"),
        Some(&Value::String("missing_grant".to_string()))
    );
}

#[test]
fn adapter_distinguishes_invalid_request_from_authorization_denial() {
    let subject = AuthzenSubject {
        r#type: "unknown-principal-kind".to_string(),
        id: "something".to_string(),
        properties: None,
    };

    let error = pdp()
        .evaluate(&request(subject, "STRIPE_KEY"), t(1_500))
        .unwrap_err();

    assert_eq!(
        error,
        AuthzenError::UnsupportedSubjectType("unknown-principal-kind".to_string())
    );
}

#[test]
fn unknown_json_fields_are_ignored_for_forward_compatibility() {
    let value = json!({
        "subject": {"type": "agent", "id": "agent:billing", "future": true},
        "action": {"name": "read", "future": {"nested": 1}},
        "resource": {"type": "secret", "id": "STRIPE_KEY", "future": [1, 2]},
        "context": {"trace_id": "trace-123"},
        "future_top_level": "ignored"
    });

    let parsed: AccessEvaluationRequest = serde_json::from_value(value).unwrap();
    assert_eq!(parsed.subject.id, "agent:billing");
    assert_eq!(parsed.action.name, "read");
    assert_eq!(parsed.resource.id, "STRIPE_KEY");
}
