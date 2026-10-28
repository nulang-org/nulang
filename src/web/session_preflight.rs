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

/// Ordinary GET/HEAD handler execution requires explicit *read* authority,
/// which can be stronger than permission to see preflight field diagnostics.
pub trait ResourceReadAuthorization {
    fn can_read(&self, principal: &str, tenant: &str, resource: &str) -> Result<bool, ()>;
}

/// Host-provided permission to execute a mutation for one exact route
/// pattern and method. Preflight/read authority MUST NOT grant write access.
pub trait ResourceWriteAuthorization {
    fn can_mutate(
        &self,
        principal: &str,
        tenant: &str,
        resource: &str,
        method: &str,
        route_pattern: &str,
    ) -> Result<bool, ()>;
}

/// Admission outcome for an authenticated, read-only HTTP route.
/// Internal credentials, principal IDs and tenant IDs must not be reflected
/// in HTTP responses.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ReadScopeError {
    Unauthenticated,
    Forbidden,
    Unavailable,
}

/// Read-only session admission is intentionally independent from form/CSRF
/// preflight. POST/PUT/PATCH/DELETE execution is *not* permitted by this API.
/// The host MUST validate request Host before invoking this function.
pub fn authorized_read_scope<S: SessionLookup + ?Sized, A: ResourceReadAuthorization + ?Sized>(
    sessions: &S,
    resource_authorizer: &A,
    headers: &[(String, String)],
    now_utc_seconds: u64,
    resource_tenant: &str,
    resource_id: &str,
) -> Result<VerifiedRequestScope, ReadScopeError> {
    let cookie = session_cookie_handle(headers).map_err(|_| ReadScopeError::Unauthenticated)?;
    let session = sessions
        .lookup(cookie)
        .map_err(|_| ReadScopeError::Unavailable)?
        .ok_or(ReadScopeError::Unauthenticated)?;
    if !canonical_id(&session.principal_id)
        || !canonical_id(&session.tenant_id)
        || session.expires_at_unix_seconds <= now_utc_seconds
    {
        return Err(ReadScopeError::Unauthenticated);
    }
    if !canonical_id(resource_tenant)
        || !canonical_id(resource_id)
        || session.tenant_id != resource_tenant
    {
        return Err(ReadScopeError::Forbidden);
    }
    match resource_authorizer.can_read(&session.principal_id, &session.tenant_id, resource_id) {
        Ok(true) => {}
        Ok(false) => return Err(ReadScopeError::Forbidden),
        Err(_) => return Err(ReadScopeError::Unavailable),
    }
    VerifiedRequestScope::from_verified_identity(session.principal_id, Some(session.tenant_id))
        .ok_or(ReadScopeError::Unauthenticated)
}

/// Host-controlled selector maps route captures to a resource and its tenant.
/// This selects *what is being requested*, not proof of ownership; a separately
/// trusted ResourceAuthorization implementation must grant the resource.
pub type SessionResourceSelector = dyn Fn(&crate::runtime::HttpRequest, &HashMap<String, String>) -> Option<(String, String)>
    + Send
    + Sync
    + 'static;

/// Optional explicitly installed read-only session adapter. The session
/// provider must verify revocation on each lookup; no cache is assumed here.
/// The built-in development listener installs no adapter by default.
pub struct SessionReadGateway {
    sessions: std::sync::Arc<dyn SessionLookup + Send + Sync>,
    resource_authorizer: std::sync::Arc<dyn ResourceReadAuthorization + Send + Sync>,
    select_resource: std::sync::Arc<SessionResourceSelector>,
}

impl SessionReadGateway {
    pub fn new(
        sessions: std::sync::Arc<dyn SessionLookup + Send + Sync>,
        resource_authorizer: std::sync::Arc<dyn ResourceReadAuthorization + Send + Sync>,
        select_resource: std::sync::Arc<SessionResourceSelector>,
    ) -> Self {
        Self {
            sessions,
            resource_authorizer,
            select_resource,
        }
    }

    pub fn admit(
        &self,
        request: &crate::runtime::HttpRequest,
        captures: &HashMap<String, String>,
    ) -> Result<VerifiedRequestScope, ReadScopeError> {
        let (tenant, resource) =
            (self.select_resource)(request, captures).ok_or(ReadScopeError::Forbidden)?;
        let now = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .map_err(|_| ReadScopeError::Unavailable)?
            .as_secs();
        authorized_read_scope(
            self.sessions.as_ref(),
            self.resource_authorizer.as_ref(),
            &request.headers,
            now,
            &tenant,
            &resource,
        )
    }
}

/// Authenticate and authorize a single mutation before calling any handler.
///
/// Host/Origin/Fetch Metadata validation MUST happen before this method. This
/// revalidates revocation, expiration, tenant, synchronizer CSRF, and route-
/// specific write authority on *every* dispatch. The host must re-check
/// business authorization, versions and idempotency in its durable transaction
/// before committing state changes; this function is not a DB transaction.
pub fn authorized_mutation_scope<
    S: SessionLookup + ?Sized,
    A: ResourceWriteAuthorization + ?Sized,
>(
    sessions: &S,
    resource_authorizer: &A,
    headers: &[(String, String)],
    body: &[u8],
    now_utc_seconds: u64,
    resource_tenant: &str,
    resource_id: &str,
    method: &str,
    route_pattern: &str,
) -> Result<VerifiedRequestScope, ReadScopeError> {
    if !matches!(method, "POST" | "PUT" | "PATCH" | "DELETE") || route_pattern.is_empty() {
        return Err(ReadScopeError::Forbidden);
    }
    let cookie = session_cookie_handle(headers).map_err(|_| ReadScopeError::Unauthenticated)?;
    let session = sessions
        .lookup(cookie)
        .map_err(|_| ReadScopeError::Unavailable)?
        .ok_or(ReadScopeError::Unauthenticated)?;
    if !canonical_id(&session.principal_id)
        || !canonical_id(&session.tenant_id)
        || session.expires_at_unix_seconds <= now_utc_seconds
    {
        return Err(ReadScopeError::Unauthenticated);
    }
    if !canonical_id(resource_tenant)
        || !canonical_id(resource_id)
        || session.tenant_id != resource_tenant
    {
        return Err(ReadScopeError::Forbidden);
    }

    // Use original raw bytes rather than a parsed HashMap: repeated/aliased
    // synchronizer fields must not collapse into one apparently valid token.
    let candidates =
        capture_submitted_csrf_tokens(headers, body).map_err(|_| ReadScopeError::Forbidden)?;
    let candidates: Vec<_> = candidates.iter().map(String::as_str).collect();
    session
        .csrf
        .verify_presented(&candidates)
        .map_err(|_| ReadScopeError::Forbidden)?;

    match resource_authorizer.can_mutate(
        &session.principal_id,
        &session.tenant_id,
        resource_id,
        method,
        route_pattern,
    ) {
        Ok(true) => {}
        Ok(false) => return Err(ReadScopeError::Forbidden),
        Err(_) => return Err(ReadScopeError::Unavailable),
    }
    VerifiedRequestScope::from_verified_identity(session.principal_id, Some(session.tenant_id))
        .ok_or(ReadScopeError::Unauthenticated)
}

/// Trusted session resolver for guarded state-changing route handlers.
/// A host must supply an authoritative, revocation-aware SessionLookup and a
/// separately audited mutation policy, not user-controlled claims.
pub struct SessionMutationGateway {
    sessions: std::sync::Arc<dyn SessionLookup + Send + Sync>,
    resource_authorizer: std::sync::Arc<dyn ResourceWriteAuthorization + Send + Sync>,
    select_resource: std::sync::Arc<SessionResourceSelector>,
}

impl SessionMutationGateway {
    pub fn new(
        sessions: std::sync::Arc<dyn SessionLookup + Send + Sync>,
        resource_authorizer: std::sync::Arc<dyn ResourceWriteAuthorization + Send + Sync>,
        select_resource: std::sync::Arc<SessionResourceSelector>,
    ) -> Self {
        Self {
            sessions,
            resource_authorizer,
            select_resource,
        }
    }

    pub fn admit(
        &self,
        request: &crate::runtime::HttpRequest,
        captures: &HashMap<String, String>,
        route_pattern: &str,
    ) -> Result<VerifiedRequestScope, ReadScopeError> {
        let (tenant, resource) =
            (self.select_resource)(request, captures).ok_or(ReadScopeError::Forbidden)?;
        let now = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .map_err(|_| ReadScopeError::Unavailable)?
            .as_secs();
        authorized_mutation_scope(
            self.sessions.as_ref(),
            self.resource_authorizer.as_ref(),
            &request.headers,
            &request.body,
            now,
            &tenant,
            &resource,
            request.method.as_str(),
            route_pattern,
        )
    }
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
