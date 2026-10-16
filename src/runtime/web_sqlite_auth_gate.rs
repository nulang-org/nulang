//! Opt-in host authorization boundary for native Web SQLite storage.
//!
//! Authorization occurs before a caller receives a tenant-scoped handle.
//! Request-provided tenant headers are *never* used to select the tenant.
//! The verifier must authenticate the bearer credential and resolve the
//! canonical tenant from trusted identity/authorization state.

use std::io;

use super::WebSqliteStore;

const MAX_BEARER_TOKEN_BYTES: usize = 4096;
const MAX_TENANT_ID_BYTES: usize = 256;

/// A trusted native host implementation that authenticates a credential,
/// validates tenant membership, and returns the canonical permitted tenant.
///
/// Do not implement this by returning a raw HTTP header, query parameter,
/// request body field, or unverified JWT claim. A rejected or expired token
/// must return Ok(None); infrastructure failure should return Err(...).
pub trait WebTenantTokenVerifier: Send + Sync {
    fn verified_tenant(&self, bearer_token: &str) -> io::Result<Option<String>>;
}

/// Explicit dependency on a storage instance and a trusted identity verifier.
/// There is no insecure/no-auth default or fallback to the global web KV.
pub struct WebSqliteAuthGate<'a> {
    store: &'a WebSqliteStore,
    verifier: &'a dyn WebTenantTokenVerifier,
}

/// A request-scoped capability. No caller-selected tenant parameter exists on
/// its CRUD surface, and the underlying store reference/tenant are private.
pub struct AuthorizedWebSqlite<'a> {
    store: &'a WebSqliteStore,
    tenant_id: String,
}

impl<'a> WebSqliteAuthGate<'a> {
    pub fn new(store: &'a WebSqliteStore, verifier: &'a dyn WebTenantTokenVerifier) -> Self {
        Self { store, verifier }
    }

    /// Authenticate a request's *sole* Authorization: Bearer header.
    ///
    /// These pairs must come from the native HTTP parser, not a permissive
    /// concatenation that loses duplicate Authorization fields.
    /// Tenant-selection headers are intentionally ignored, including
    /// X-Tenant-ID and X-Forwarded-Tenant.
    pub fn authorize_headers(
        &self,
        headers: &[(String, String)],
    ) -> io::Result<AuthorizedWebSqlite<'a>> {
        let mut authorization = None;
        for (name, value) in headers {
            if name.eq_ignore_ascii_case("authorization") {
                if authorization.replace(value.as_str()).is_some() {
                    return Err(denied());
                }
            }
        }
        let value = authorization.ok_or_else(denied)?;
        let (scheme, token) = value.split_once(' ').ok_or_else(denied)?;
        if !scheme.eq_ignore_ascii_case("bearer")
            || token.is_empty()
            || token.len() > MAX_BEARER_TOKEN_BYTES
            || !token.bytes().all(|b| b.is_ascii_graphic() && b != b',')
        {
            return Err(denied());
        }

        // Only the verifier establishes tenant membership. Never trust
        // caller-controlled tenant IDs, including inputs in other headers.
        let tenant_id = self
            .verifier
            .verified_tenant(token)
            .map_err(|_| io::Error::other("web tenant verification unavailable"))?
            .ok_or_else(denied)?;

        if tenant_id.is_empty()
            || tenant_id.len() > MAX_TENANT_ID_BYTES
            || tenant_id.trim() != tenant_id
            || tenant_id.chars().any(char::is_control)
        {
            return Err(denied());
        }

        Ok(AuthorizedWebSqlite {
            store: self.store,
            tenant_id,
        })
    }
}

impl AuthorizedWebSqlite<'_> {
    pub fn get(&self, key: &str) -> io::Result<Option<String>> {
        self.store.get(&self.tenant_id, key)
    }

    pub fn put(&self, key: &str, value: &str) -> io::Result<()> {
        self.store.put(&self.tenant_id, key, value)
    }

    pub fn delete(&self, key: &str) -> io::Result<bool> {
        self.store.delete(&self.tenant_id, key)
    }

    pub fn list(&self, limit: usize) -> io::Result<Vec<(String, String)>> {
        self.store.list(&self.tenant_id, limit)
    }
}

fn denied() -> io::Error {
    // Constant user-visible error for absent, malformed, duplicated,
    // expired, and denied credentials: no token, tenant, or claim leakage.
    io::Error::new(io::ErrorKind::PermissionDenied, "web SQLite access denied")
}
