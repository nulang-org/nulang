//! Read-only authenticated cookie-session + tenant/resource preflight boundary.
//!
//! This library function is NOT an HTTP handler, authentication provider,
//! PostgreSQL RLS implementation or production admission policy. The caller
//! must first enforce the audited Host, Origin and Fetch Metadata rules.
//! Only a trusted server-side SessionLookup may supply a TrustedSession.

use crate::web::bindings::RouteBindingContract;
use crate::web::csrf::SessionCsrfToken;
use crate::web::http_request::{capture_submitted_csrf_tokens, HttpRequestBindingInputs};
use crate::web::request_bindings::{
    preflight_request_arguments, RequestDecodeError, RequestPreflight, VerifiedRequestScope,
};
use std::collections::HashMap;

/// Host-only, Secure, Path=/, no-Domain cookie name when provisioning sessions.
/// The framework does NOT set the cookie or create sessions here.
pub const SESSION_COOKIE_NAME: &str = "__Host-nulang_session";

/// Data retrieved from a server-controlled, authenticated session backend.
///
/// Never populate from a request field, token claim that has not been verified,
/// or client-side form state. The CSRF token must be retained server-side and
/// rotated on changes to authentication or privilege.
#[derive(Clone)]
pub struct TrustedSession {
    principal_id: String,
    tenant_id: String,
    expires_at_unix_seconds: u64,
    csrf: SessionCsrfToken,
}

impl TrustedSession {
    pub fn new(
        principal_id: impl Into<String>,
        tenant_id: impl Into<String>,
        expires_at_unix_seconds: u64,
        csrf: SessionCsrfToken,
    ) -> Self {
        Self {
            principal_id: principal_id.into(),
            tenant_id: tenant_id.into(),
            expires_at_unix_seconds,
            csrf,
        }
    }
}

/// The only authority to translate an opaque cookie into an authenticated
/// session record. Implement against a trusted, revocation-aware session store.
/// Err(()) denotes an unavailable backend; do not reinterpret it as a guest.
pub trait SessionLookup {
    fn lookup(&self, opaque_cookie: &str) -> Result<Option<TrustedSession>, ()>;
}

/// Independently verify that this principal can read validation information
/// for the *specified* resource, within its server-established tenant.
/// A tenant membership alone is insufficient for per-resource authorization.
pub trait ResourceAuthorization {
    fn can_preflight(&self, principal: &str, tenant: &str, resource: &str) -> Result<bool, ()>;
}

/// Never serialize variant details to clients. Map Denied to one generic 403,
/// Unavailable to generic 503, and InvalidPlan to generic 500.
#[derive(Debug)]
pub enum AdmissionError {
    Denied,
    Unavailable,
    InvalidPlan(RequestDecodeError),
}

/// Return a strictly parsed opaque cookie handle; never percent-decode it.
/// Multiple Cookie headers, repeated session-cookie names, malformed token
/// characters, and missing session credentials all fail closed.
fn session_cookie_handle(headers: &[(String, String)]) -> Result<&str, AdmissionError> {
    let mut cookies = headers
        .iter()
        .filter(|(key, _)| key.eq_ignore_ascii_case("cookie"));
    let (_, header) = cookies.next().ok_or(AdmissionError::Denied)?;
    if cookies.next().is_some() {
        return Err(AdmissionError::Denied);
    }
    let mut selected = None;
    for part in header.split(';') {
        let (name, raw) = match part.trim().split_once('=') {
            Some(pair) => pair,
            None => return Err(AdmissionError::Denied),
        };
        if name == SESSION_COOKIE_NAME {
            if selected.replace(raw).is_some() {
                return Err(AdmissionError::Denied);
            }
        }
    }
    let handle = selected.ok_or(AdmissionError::Denied)?;
    // 32 random bytes encoded as unpadded base64url => 43 ASCII characters.
    // A cookie handle is untrusted until the server-side lookup succeeds.
    if handle.len() != 43
        || !handle
            .bytes()
            .all(|c| c.is_ascii_alphanumeric() || c == b'-' || c == b'_')
    {
        return Err(AdmissionError::Denied);
    }
    Ok(handle)
}

/// Conservative canonical ID constraints. This does not interpret tenant
/// authority: it only rejects nonsensical or ambiguous identifiers.
fn canonical_id(s: &str) -> bool {
    !s.is_empty()
        && s.len() <= 128
        && s.bytes()
            .all(|c| c.is_ascii_alphanumeric() || c == b'-' || c == b'_')
}

/// Compose a *server-owned authenticated session* with CSRF and a separate
/// resource authorization decision before any field diagnostics are returned.
///
/// Preconditions: the HTTP host has rejected invalid Host/Origin/Fetch Metadata,
/// enforced request limits and authenticated its request transport. Do not
/// expose this function as a public route without those explicit controls.
/// Neither tenant nor principal is ever read from request headers/form values.
/// The resource tenant must be resolved by trusted routing/domain code and
/// checked against the server-owned session; resource_authorizer must check
/// the requested resource independently, preferably within the authoritative
/// tenant/RLS database boundary. The clock is server-owned UTC seconds.
///
/// This performs no effects, handler calls, database writes or UI mutation.
/// Actual submissions must repeat all authz, CSRF, business validation and
/// concurrency/version checks in the authoritative transaction.
pub fn authorized_session_preflight<S: SessionLookup, A: ResourceAuthorization>(
    sessions: &S,
    resource_authorizer: &A,
    headers: &[(String, String)],
    body: &[u8],
    request_target: &str,
    route_captures: &HashMap<String, String>,
    now_utc_seconds: u64,
    resource_tenant: &str,
    resource_id: &str,
    bindings: &[RouteBindingContract],
    handler_param_count: usize,
) -> Result<RequestPreflight, AdmissionError> {
    let opaque_handle = session_cookie_handle(headers)?;
    let session = sessions
        .lookup(opaque_handle)
        .map_err(|_| AdmissionError::Unavailable)?
        .ok_or(AdmissionError::Denied)?;

    if !canonical_id(&session.principal_id)
        || !canonical_id(&session.tenant_id)
        || !canonical_id(resource_tenant)
        || !canonical_id(resource_id)
        || session.expires_at_unix_seconds <= now_utc_seconds
        || session.tenant_id != resource_tenant
    {
        return Err(AdmissionError::Denied);
    }

    let candidates =
        capture_submitted_csrf_tokens(headers, body).map_err(|_| AdmissionError::Denied)?;
    let references: Vec<&str> = candidates.iter().map(String::as_str).collect();
    session
        .csrf
        .verify_presented(&references)
        .map_err(|_| AdmissionError::Denied)?;

    let allowed = resource_authorizer
        .can_preflight(&session.principal_id, &session.tenant_id, resource_id)
        .map_err(|_| AdmissionError::Unavailable)?;
    if !allowed {
        return Err(AdmissionError::Denied);
    }

    // Only after session expiry, tenant boundary, synchronizer CSRF, and
    // resource authorization have succeeded may identity become a trusted
    // compiler scope binding. Never derive it from request path/form/headers.
    let scope =
        VerifiedRequestScope::from_verified_identity(session.principal_id, Some(session.tenant_id))
            .ok_or(AdmissionError::Denied)?;

    // Capture the exact same raw request that passed admission; do not
    // accept a caller-supplied RequestBindingValues from another request.
    let captured = HttpRequestBindingInputs::capture(request_target, headers, body);
    let mut values = captured.values(route_captures, headers);
    values.scope = Some(&scope);
    preflight_request_arguments(bindings, handler_param_count, &values)
        .map_err(AdmissionError::InvalidPlan)
}
