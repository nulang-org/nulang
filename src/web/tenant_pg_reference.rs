//! Disposable PostgreSQL request-authority reference, not a production
//! authentication provider or a public Nulang Web route adapter.
//!
//! It consumes an identity supplied by a separately trusted token verifier,
//! then resolves database credentials from server-owned membership bindings.
//! It *never* trusts tenant IDs in URL, body or headers. Connections are
//! per-request and transactions commit only after a successful handler result.
//!
//! For security, this implementation refuses all non-pg_virtualenv clusters,
//! remote database hosts and PostgreSQL's default port. No TLS/production
//! credential lifecycle or OIDC provider is implemented here.

use crate::runtime::HttpRequest;
use postgres::{Config, NoTls, Transaction};
use std::collections::HashMap;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum TenantAuthorityError {
    Unauthorized,
    InvalidConfiguration,
    NotDisposable,
    RoleMismatch,
    Database,
}

/// The caller is responsible for independent, cryptographic verification:
/// signed token, issuer, audience, expiry, key rotation and revocation.
/// Implementations that return a subject from an unverified header or cookie
/// are unsafe. The regression test injects a fake server-owned token map.
pub trait TrustedTokenVerifier: Send + Sync {
    fn verify(&self, bearer: &str) -> Option<String>;
}

/// Server-only mapping of a verified identity to exactly one tenant/DB login.
/// Do not log or serialize this structure: it includes a database password.
pub struct TenantRoleBinding {
    tenant_id: String,
    login: String,
    password: String,
}
impl TenantRoleBinding {
    pub fn new(tenant_id: &str, login: &str, password: String) -> Self {
        Self {
            tenant_id: tenant_id.to_string(),
            login: login.to_string(),
            password,
        }
    }
}

/// Strictly local, disposable reference. A deployment-ready adapter would
/// need an audited token verifier, membership revocation, TLS and pooling.
pub struct TenantPostgresAuthority<V> {
    host: String,
    port: u16,
    verifier: V,
    memberships: HashMap<String, TenantRoleBinding>,
}

impl<V: TrustedTokenVerifier> TenantPostgresAuthority<V> {
    pub fn new(
        host: &str,
        port: u16,
        verifier: V,
        memberships: HashMap<String, TenantRoleBinding>,
    ) -> Result<Self, TenantAuthorityError> {
        if host != "127.0.0.1" || port == 0 || port == 5432 {
            return Err(TenantAuthorityError::InvalidConfiguration);
        }
        if memberships.values().any(|binding| {
            binding.tenant_id.is_empty() || binding.login.is_empty() || binding.password.is_empty()
        }) {
            return Err(TenantAuthorityError::InvalidConfiguration);
        }
        Ok(Self {
            host: host.to_string(),
            port,
            verifier,
            memberships,
        })
    }

    /// Authorize one request and run its SQL unit of work on a **new** DB
    /// connection; no client-side choice of tenant or PostgreSQL role exists.
    /// Even if the handler returns an error after a write, dropping the
    /// uncommitted transaction rolls the operation back.
    pub fn execute<R, F>(
        &self,
        request: &HttpRequest,
        handler: F,
    ) -> Result<R, TenantAuthorityError>
    where
        F: FnOnce(&mut Transaction<'_>) -> Result<R, postgres::Error>,
    {
        let bearer = bearer_token(&request.headers).ok_or(TenantAuthorityError::Unauthorized)?;
        let subject = self
            .verifier
            .verify(bearer)
            .filter(|subject| !subject.is_empty())
            .ok_or(TenantAuthorityError::Unauthorized)?;
        let binding = self
            .memberships
            .get(&subject)
            .ok_or(TenantAuthorityError::Unauthorized)?;

        // Defense in depth: not enough to set an environment variable or
        // connect to another local service with caller-provided credentials.
        let conf_root = std::env::var("PG_CLUSTER_CONF_ROOT")
            .map_err(|_| TenantAuthorityError::NotDisposable)?;
        if std::env::var("NULANG_TEST_POSTGRES_DISPOSABLE").as_deref() != Ok("1")
            || !conf_root.starts_with("/tmp/pg_virtualenv.")
            || !conf_root.ends_with("/postgresql")
        {
            return Err(TenantAuthorityError::NotDisposable);
        }

        let mut config = Config::new();
        config
            .host(&self.host)
            .port(self.port)
            .user(&binding.login)
            .password(&binding.password)
            .dbname("postgres")
            .connect_timeout(std::time::Duration::from_secs(3));
        let mut client = config
            .connect(NoTls)
            .map_err(|_| TenantAuthorityError::Database)?;

        // Ordinary tenant roles must NOT be granted pg_read_all_settings
        // merely to inspect config_file. The privileged disposable wrapper
        // checks that setting before creating any fixture objects. Tenant
        // sessions can independently verify the connected network endpoint.
        let endpoint = client
            .query_one(
                "SELECT pg_catalog.host(inet_server_addr()), inet_server_port()",
                &[],
            )
            .map_err(|_| TenantAuthorityError::Database)?;
        let connected_host: Option<String> = endpoint.get(0);
        let connected_port: Option<i32> = endpoint.get(1);
        if connected_host.as_deref() != Some("127.0.0.1")
            || connected_port != Some(i32::from(self.port))
        {
            return Err(TenantAuthorityError::NotDisposable);
        }

        let row = client
            .query_one(
                "SELECT session_user::text, current_user::text, \
                 tenant_example_private.current_tenant(), \
                 rolbypassrls, rolsuper \
                 FROM pg_catalog.pg_roles WHERE rolname = session_user::name",
                &[],
            )
            .map_err(|_| TenantAuthorityError::Database)?;
        let session_user: String = row.get(0);
        let current_user: String = row.get(1);
        let actual_tenant: Option<String> = row.get(2);
        let bypass_rls: bool = row.get(3);
        let superuser: bool = row.get(4);
        if session_user != binding.login
            || current_user != binding.login
            || actual_tenant.as_deref() != Some(&binding.tenant_id)
            || bypass_rls
            || superuser
        {
            return Err(TenantAuthorityError::RoleMismatch);
        }

        let mut tx = client
            .transaction()
            .map_err(|_| TenantAuthorityError::Database)?;
        let result = handler(&mut tx).map_err(|_| TenantAuthorityError::Database)?;
        tx.commit().map_err(|_| TenantAuthorityError::Database)?;
        Ok(result)
    }
}

fn bearer_token(headers: &[(String, String)]) -> Option<&str> {
    let mut found = None;
    for (name, value) in headers {
        if !name.eq_ignore_ascii_case("authorization") {
            continue;
        }
        if found.is_some() {
            return None;
        }
        let bearer = value.strip_prefix("Bearer ")?;
        if bearer.is_empty()
            || bearer.len() > 4096
            || !bearer.bytes().all(|byte| byte.is_ascii_graphic())
        {
            return None;
        }
        found = Some(bearer);
    }
    found
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn reject_remote_default_or_empty_credentials() {
        struct NoToken;
        impl TrustedTokenVerifier for NoToken {
            fn verify(&self, _bearer: &str) -> Option<String> {
                None
            }
        }
        assert!(matches!(
            TenantPostgresAuthority::new("db.prod.example", 15432, NoToken, HashMap::new()),
            Err(TenantAuthorityError::InvalidConfiguration)
        ));
        assert!(matches!(
            TenantPostgresAuthority::new("127.0.0.1", 5432, NoToken, HashMap::new()),
            Err(TenantAuthorityError::InvalidConfiguration)
        ));
    }

    #[test]
    fn reject_malformed_and_duplicate_bearer_credentials() {
        assert_eq!(bearer_token(&[]), None);
        assert_eq!(
            bearer_token(&[("Authorization".into(), "Bearer token".into())]),
            Some("token")
        );
        assert_eq!(
            bearer_token(&[
                ("Authorization".into(), "Bearer one".into()),
                ("authorization".into(), "Bearer two".into())
            ]),
            None
        );
        assert_eq!(
            bearer_token(&[(
                "Authorization".into(),
                "Bearer forged\r\nX-Tenant: B".into()
            )]),
            None
        );
    }
}
