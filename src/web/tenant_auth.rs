//! Opt-in, server-owned tenant session admission for experimental Web routes.
//!
//! The resolver MUST be backed by an independently authenticated, unexpired
//! server-side session and checked tenant membership. A cookie or tenant id
//! by itself is not proof. The resolver is invoked only after strict cookie
//! extraction. A failed resolver denies the route before handler dispatch.

use crate::runtime::HttpMethod;
use crate::web::runtime_bindings::RuntimeWebRoute;
use std::sync::Arc;

/// Principal constructed by a trusted session+membership verifier.
/// Never derive this from request parameters, headers, or unverified claims.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct VerifiedTenant {
    subject_id: String,
    tenant_id: String,
}

impl VerifiedTenant {
    pub fn new(subject_id: &str, tenant_id: &str) -> Option<Self> {
        fn safe_id(value: &str) -> bool {
            !value.is_empty()
                && value.len() <= 128
                && value
                    .bytes()
                    .all(|b| b.is_ascii_alphanumeric() || b == b'-' || b == b'_')
        }
        if !safe_id(subject_id) || !safe_id(tenant_id) {
            return None;
        }
        Some(Self {
            subject_id: subject_id.into(),
            tenant_id: tenant_id.into(),
        })
    }

    pub fn subject_id(&self) -> &str {
        &self.subject_id
    }
    pub fn tenant_id(&self) -> &str {
        &self.tenant_id
    }
}

/// Resolver contract. Token lookup must enforce expiration, revocation,
/// authenticated user identity, and current tenant membership, fail closed
/// on backend errors, and never treat a client-provided tenant id as authority.
pub type TenantSessionResolver = dyn Fn(&str) -> Option<VerifiedTenant> + Send + Sync;

#[derive(Clone)]
pub struct TenantAuthGuard {
    cookie_name: String,
    protected_routes: Vec<(HttpMethod, String)>,
    resolver: Arc<TenantSessionResolver>,
}

impl std::fmt::Debug for TenantAuthGuard {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("TenantAuthGuard")
            .field("cookie_name", &self.cookie_name)
            .field("protected_routes", &self.protected_routes)
            .field("resolver", &"[redacted]")
            .finish()
    }
}

/// Conservative route-overlap check. A broader unguarded route must not
/// intercept a guarded route even if it was registered earlier. Rejecting a
/// possible overlap is safer than trusting route order for authentication.
fn potentially_overlapping_routes(left: &str, right: &str) -> bool {
    // Wildcard catch-all route syntax varies by transport; any wildcard
    // on this method is conservatively considered a possible overlap.
    if left.contains('*') || right.contains('*') {
        return true;
    }
    let left = left.trim_matches('/').split('/').collect::<Vec<_>>();
    let right = right.trim_matches('/').split('/').collect::<Vec<_>>();
    if left.len() != right.len() {
        return false;
    }
    left.iter().zip(right.iter()).all(|(a, b)| {
        a == b
            || a.starts_with(':')
            || b.starts_with(':')
            || (a.starts_with('{') && a.ends_with('}'))
            || (b.starts_with('{') && b.ends_with('}'))
    })
}

impl TenantAuthGuard {
    pub fn new(
        cookie_name: &str,
        protected_routes: Vec<(HttpMethod, String)>,
        resolver: Arc<TenantSessionResolver>,
    ) -> Result<Self, String> {
        if !cookie_name.starts_with("__Host-")
            || cookie_name.len() <= "__Host-".len()
            || !cookie_name
                .bytes()
                .all(|b| b.is_ascii_alphanumeric() || b == b'-' || b == b'_')
        {
            return Err("tenant session requires a valid __Host- cookie name".into());
        }
        if protected_routes.is_empty() {
            return Err("tenant guard requires at least one protected route".into());
        }
        let mut seen = std::collections::HashSet::new();
        for (method, path) in &protected_routes {
            if !path.starts_with('/')
                || path.starts_with("/__nulang/sse/")
                || !seen.insert((method.as_str(), path.as_str()))
            {
                return Err("invalid or duplicate protected route".into());
            }
        }
        Ok(Self {
            cookie_name: cookie_name.into(),
            protected_routes,
            resolver,
        })
    }

    pub(crate) fn validate_routes(&self, routes: &[RuntimeWebRoute]) -> Result<(), String> {
        for (method, path) in &self.protected_routes {
            if routes
                .iter()
                .filter(|route| route.route.method == *method && route.route.path == *path)
                .count()
                != 1
            {
                return Err(
                    "tenant guard requires exactly one registered route per protected method/path"
                        .into(),
                );
            }
            if routes.iter().any(|route| {
                route.route.method == *method
                    && route.route.path != *path
                    && potentially_overlapping_routes(&route.route.path, path)
            }) {
                return Err("guarded route overlaps an unguarded method/path registration".into());
            }
        }
        Ok(())
    }

    /// Reject unsafe protected routes without independent CSRF admission.
    /// Session membership proves identity, not browser request intent.
    pub(crate) fn validate_mutation_routes(
        &self,
        form_guard_keys: &std::collections::HashSet<(String, String)>,
    ) -> Result<(), String> {
        for (method, path) in &self.protected_routes {
            if !matches!(
                method,
                HttpMethod::Get | HttpMethod::Head | HttpMethod::Options
            ) && !form_guard_keys.contains(&(method.as_str().to_owned(), path.clone()))
            {
                return Err(
                    "unsafe protected tenant route requires compiler-validated CSRF form guard"
                        .into(),
                );
            }
        }
        Ok(())
    }

    pub(crate) fn protects(&self, method: HttpMethod, path: &str) -> bool {
        self.protected_routes
            .iter()
            .any(|(m, p)| *m == method && p == path)
    }

    /// Parse only this configured opaque cookie. Ambiguous or forged values
    /// are rejected without resolver invocation, even across Cookie headers.
    pub(crate) fn resolve(&self, headers: &[(String, String)]) -> Option<VerifiedTenant> {
        let mut selected = None;
        for (name, value) in headers {
            if !name.eq_ignore_ascii_case("cookie") {
                continue;
            }
            for item in value.split(';') {
                let (name, token) = item.trim().split_once('=')?;
                if name.trim() != self.cookie_name {
                    continue;
                }
                let token = token.trim();
                if selected.is_some()
                    || token.len() != 64
                    || !token
                        .bytes()
                        .all(|b| b.is_ascii_digit() || (b'a'..=b'f').contains(&b))
                {
                    return None;
                }
                selected = Some(token);
            }
        }
        (self.resolver)(selected?)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn invalid_resolver_identity_cannot_be_used() {
        assert!(VerifiedTenant::new("", "tenant").is_none());
        assert!(VerifiedTenant::new("user", "").is_none());
        assert!(VerifiedTenant::new("user", "tenant\r\nAdmin: yes").is_none());
    }

    #[test]
    fn tenant_cookie_and_route_configuration_is_fail_closed() {
        let resolver: Arc<TenantSessionResolver> = Arc::new(|_| None);
        assert!(TenantAuthGuard::new(
            "session",
            vec![(HttpMethod::Get, "/x".into())],
            resolver.clone()
        )
        .is_err());
        assert!(TenantAuthGuard::new("__Host-a", vec![], resolver.clone()).is_err());
        assert!(TenantAuthGuard::new(
            "__Host-a",
            vec![
                (HttpMethod::Get, "/x".into()),
                (HttpMethod::Get, "/x".into())
            ],
            resolver.clone()
        )
        .is_err());
        assert!(TenantAuthGuard::new(
            "__Host-a",
            vec![(HttpMethod::Get, "/__nulang/sse/room".into())],
            resolver
        )
        .is_err());
    }
}
