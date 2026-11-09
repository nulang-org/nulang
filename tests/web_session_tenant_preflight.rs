//! Server-owned session -> same-tenant resource authority -> CSRF -> typed form.
//! All fixtures are local; this must not expose a public HTTP endpoint.
use nulang::web::bindings::{RouteBindingContract, RouteBindingSource};
use nulang::web::csrf::SessionCsrfToken;
use nulang::web::session_preflight::{
    authorized_session_preflight, AdmissionError, ResourceAuthorization, SessionLookup,
    TrustedSession, SESSION_COOKIE_NAME,
};
use std::cell::Cell;
use std::collections::HashMap;

const NOW: u64 = 1_800_000_000;
const HANDLE: &str = "aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa";
const OTHER_HANDLE: &str = "bbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbb";

struct Store {
    expected: String,
    session: Option<TrustedSession>,
    fail: bool,
    lookups: Cell<usize>,
}
impl SessionLookup for Store {
    fn lookup(&self, handle: &str) -> Result<Option<TrustedSession>, ()> {
        self.lookups.set(self.lookups.get() + 1);
        if self.fail {
            return Err(());
        }
        Ok(if handle == self.expected {
            self.session.clone()
        } else {
            None
        })
    }
}

struct Policy {
    allow: bool,
    fail: bool,
    checks: Cell<usize>,
}
impl ResourceAuthorization for Policy {
    fn can_preflight(&self, principal: &str, tenant: &str, resource: &str) -> Result<bool, ()> {
        self.checks.set(self.checks.get() + 1);
        assert_eq!(principal, "user-a");
        assert_eq!(tenant, "tenant-a");
        assert_eq!(resource, "order-4");
        if self.fail {
            Err(())
        } else {
            Ok(self.allow)
        }
    }
}
fn fixture() -> (Store, Policy, SessionCsrfToken) {
    let csrf = SessionCsrfToken::generate().unwrap();
    (
        Store {
            expected: HANDLE.into(),
            session: Some(TrustedSession::new(
                "user-a",
                "tenant-a",
                NOW + 60,
                csrf.clone(),
            )),
            fail: false,
            lookups: Cell::new(0),
        },
        Policy {
            allow: true,
            fail: false,
            checks: Cell::new(0),
        },
        csrf,
    )
}
fn headers(handle: &str, csrf: &str) -> Vec<(String, String)> {
    vec![
        (
            "Cookie".into(),
            format!("{SESSION_COOKIE_NAME}={handle}; theme=dark"),
        ),
        (
            "Content-Type".into(),
            "application/x-www-form-urlencoded".into(),
        ),
        ("X-CSRF-Token".into(), csrf.into()),
        ("X-Tenant-Id".into(), "tenant-attacker".into()),
    ]
}
fn binding() -> Vec<RouteBindingContract> {
    vec![RouteBindingContract {
        source: RouteBindingSource::Form,
        source_name: "quantity".into(),
        handler_param: "private_slot".into(),
        handler_index: 0,
        ty: Some("Int".into()),
    }]
}
fn invoke(
    store: &Store,
    policy: &Policy,
    headers: &[(String, String)],
    target_tenant: &str,
    bindings: &[RouteBindingContract],
) -> Result<nulang::web::request_bindings::RequestPreflight, AdmissionError> {
    let body = b"quantity=bad&tenant=tenant-attacker";
    let path = HashMap::new();
    authorized_session_preflight(
        store,
        policy,
        headers,
        body,
        "/orders/order-4",
        &path,
        NOW,
        target_tenant,
        "order-4",
        bindings,
        1,
    )
}
#[test]
fn authenticated_authorized_session_gets_safe_field_diagnostics_only() {
    let (store, policy, csrf) = fixture();
    let result = invoke(
        &store,
        &policy,
        &headers(HANDLE, &csrf.encode()),
        "tenant-a",
        &binding(),
    )
    .unwrap();
    assert!(!result.valid());
    assert_eq!(result.to_json()["errors"][0]["source_name"], "quantity");
    assert!(!result.to_json().to_string().contains("private_slot"));
    assert_eq!(store.lookups.get(), 1);
    assert_eq!(policy.checks.get(), 1);
}
#[test]
fn wrong_tenant_and_forged_tenant_header_do_not_override_session() {
    let (store, policy, csrf) = fixture();
    assert!(matches!(
        invoke(
            &store,
            &policy,
            &headers(HANDLE, &csrf.encode()),
            "tenant-b",
            &binding()
        ),
        Err(AdmissionError::Denied)
    ));
    assert_eq!(policy.checks.get(), 0);
}
#[test]
fn invalid_session_absent_or_expired_never_reaches_authorizer() {
    let (mut store, policy, csrf) = fixture();
    assert!(matches!(
        invoke(
            &store,
            &policy,
            &headers(OTHER_HANDLE, &csrf.encode()),
            "tenant-a",
            &binding()
        ),
        Err(AdmissionError::Denied)
    ));
    store.session = Some(TrustedSession::new("user-a", "tenant-a", NOW, csrf.clone()));
    assert!(matches!(
        invoke(
            &store,
            &policy,
            &headers(HANDLE, &csrf.encode()),
            "tenant-a",
            &binding()
        ),
        Err(AdmissionError::Denied)
    ));
    assert_eq!(policy.checks.get(), 0);
}
#[test]
fn duplicate_cookie_headers_and_duplicate_named_cookies_fail_before_lookup() {
    let (store, policy, csrf) = fixture();
    let mut h = headers(HANDLE, &csrf.encode());
    h.push(("Cookie".into(), format!("{SESSION_COOKIE_NAME}={HANDLE}")));
    assert!(matches!(
        invoke(&store, &policy, &h, "tenant-a", &binding()),
        Err(AdmissionError::Denied)
    ));
    h.pop();
    h[0].1
        .push_str(&format!("; {SESSION_COOKIE_NAME}={HANDLE}"));
    assert!(matches!(
        invoke(&store, &policy, &h, "tenant-a", &binding()),
        Err(AdmissionError::Denied)
    ));
    assert_eq!(store.lookups.get(), 0);
}
#[test]
fn missing_or_ambiguous_csrf_denies_before_authorization() {
    let (store, policy, csrf) = fixture();
    let mut h = headers(HANDLE, &csrf.encode());
    h.retain(|(name, _)| name != "X-CSRF-Token");
    assert!(matches!(
        invoke(&store, &policy, &h, "tenant-a", &binding()),
        Err(AdmissionError::Denied)
    ));
    h.push(("X-CSRF-Token".into(), csrf.encode()));
    h.push(("x-csrf-token".into(), csrf.encode()));
    assert!(matches!(
        invoke(&store, &policy, &h, "tenant-a", &binding()),
        Err(AdmissionError::Denied)
    ));
    assert_eq!(policy.checks.get(), 0);
}
#[test]
fn denied_resource_or_backend_failure_cannot_emit_form_errors() {
    let (store, mut policy, csrf) = fixture();
    let h = headers(HANDLE, &csrf.encode());
    policy.allow = false;
    assert!(matches!(
        invoke(&store, &policy, &h, "tenant-a", &binding()),
        Err(AdmissionError::Denied)
    ));
    policy.fail = true;
    assert!(matches!(
        invoke(&store, &policy, &h, "tenant-a", &binding()),
        Err(AdmissionError::Unavailable)
    ));
    let mut failing_store = store;
    failing_store.fail = true;
    assert!(matches!(
        invoke(&failing_store, &policy, &h, "tenant-a", &binding()),
        Err(AdmissionError::Unavailable)
    ));
}
#[test]
fn malformed_plan_returns_internal_error_only_after_authorization() {
    let (store, policy, csrf) = fixture();
    let h = headers(HANDLE, &csrf.encode());
    assert!(matches!(
        invoke(&store, &policy, &h, "tenant-a", &[]),
        Err(AdmissionError::InvalidPlan(_))
    ));
    assert_eq!(policy.checks.get(), 1);
}

#[test]
fn query_and_form_values_are_decoded_from_the_admitted_request() {
    let (store, policy, csrf) = fixture();
    let h = headers(HANDLE, &csrf.encode());
    let route_captures = HashMap::new();
    let bindings = [
        RouteBindingContract {
            source: RouteBindingSource::Query,
            source_name: "limit".into(),
            handler_param: "limit".into(),
            handler_index: 0,
            ty: Some("Int".into()),
        },
        RouteBindingContract {
            source: RouteBindingSource::Form,
            source_name: "quantity".into(),
            handler_param: "quantity".into(),
            handler_index: 1,
            ty: Some("Int".into()),
        },
    ];
    let first = authorized_session_preflight(
        &store,
        &policy,
        &h,
        b"quantity=2",
        "/orders/order-4?limit=bogus",
        &route_captures,
        NOW,
        "tenant-a",
        "order-4",
        &bindings,
        2,
    )
    .unwrap();
    assert_eq!(first.errors.len(), 1);
    assert_eq!(first.to_json()["errors"][0]["source"], "query");
    assert_eq!(first.to_json()["errors"][0]["source_name"], "limit");

    let fixed = authorized_session_preflight(
        &store,
        &policy,
        &h,
        b"quantity=2",
        "/orders/order-4?limit=7",
        &route_captures,
        NOW,
        "tenant-a",
        "order-4",
        &bindings,
        2,
    )
    .unwrap();
    assert!(fixed.valid());
}

#[test]
fn malformed_or_missing_cookie_is_rejected_before_session_lookup() {
    let (store, policy, csrf) = fixture();
    let mut h = headers(HANDLE, &csrf.encode());
    h.retain(|(key, _)| key != "Cookie");
    assert!(matches!(
        invoke(&store, &policy, &h, "tenant-a", &binding()),
        Err(AdmissionError::Denied)
    ));
    h.push(("Cookie".into(), format!("{SESSION_COOKIE_NAME}=short")));
    assert!(matches!(
        invoke(&store, &policy, &h, "tenant-a", &binding()),
        Err(AdmissionError::Denied)
    ));
    assert_eq!(store.lookups.get(), 0);
    assert_eq!(policy.checks.get(), 0);
}

#[test]
fn trusted_scope_from_authorized_session_cannot_be_replaced_by_request_data() {
    // This regression joins the two independent features: compiler-owned
    // from scope(...) bindings and the session/tenant/CSRF preflight boundary.
    let (store, policy, csrf) = fixture();
    let mut fields = binding();
    fields.push(RouteBindingContract {
        source: RouteBindingSource::Scope,
        source_name: "principal_id".into(),
        handler_param: "principal".into(),
        handler_index: 1,
        ty: Some("String".into()),
    });
    fields.push(RouteBindingContract {
        source: RouteBindingSource::Scope,
        source_name: "tenant_id".into(),
        handler_param: "tenant".into(),
        handler_index: 2,
        ty: Some("String".into()),
    });
    let forged = headers(HANDLE, &csrf.encode());
    let result = authorized_session_preflight(
        &store,
        &policy,
        &forged,
        b"quantity=bad&tenant_id=attacker&principal_id=attacker",
        "/orders/order-4?tenant_id=attacker",
        &HashMap::from([("tenant_id".into(), "attacker".into())]),
        NOW,
        "tenant-a",
        "order-4",
        &fields,
        3,
    )
    .expect("server-verified scope must be available despite forged request fields");
    assert!(!result.valid()); // quantity is intentionally malformed
    assert_eq!(result.errors.len(), 1);
    assert_eq!(result.errors[0].source_name.as_deref(), Some("quantity"));
    assert_eq!(policy.checks.get(), 1);
}

#[test]
fn wrong_tenant_with_scope_fails_before_form_diagnostics() {
    let (store, policy, csrf) = fixture();
    let mut fields = binding();
    fields.push(RouteBindingContract {
        source: RouteBindingSource::Scope,
        source_name: "tenant_id".into(),
        handler_param: "tenant".into(),
        handler_index: 1,
        ty: Some("String".into()),
    });
    let error = authorized_session_preflight(
        &store,
        &policy,
        &headers(HANDLE, &csrf.encode()),
        b"quantity=not-an-int&tenant_id=attacker",
        "/orders/order-4",
        &HashMap::new(),
        NOW,
        "tenant-b",
        "order-4",
        &fields,
        2,
    )
    .unwrap_err();
    assert!(matches!(error, AdmissionError::Denied));
    assert_eq!(policy.checks.get(), 0);
}

#[test]
fn unavailable_session_store_never_exposes_scope_or_form_errors() {
    let (mut store, policy, csrf) = fixture();
    store.fail = true;
    let mut fields = binding();
    fields.push(RouteBindingContract {
        source: RouteBindingSource::Scope,
        source_name: "principal_id".into(),
        handler_param: "principal".into(),
        handler_index: 1,
        ty: Some("String".into()),
    });
    let error = authorized_session_preflight(
        &store,
        &policy,
        &headers(HANDLE, &csrf.encode()),
        b"quantity=bad",
        "/orders/order-4",
        &HashMap::new(),
        NOW,
        "tenant-a",
        "order-4",
        &fields,
        2,
    )
    .unwrap_err();
    assert!(matches!(error, AdmissionError::Unavailable));
    assert_eq!(policy.checks.get(), 0);
}
