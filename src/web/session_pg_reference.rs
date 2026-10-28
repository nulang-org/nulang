//! Disposable PostgreSQL reference adapters for server-owned cookie sessions
//! and per-tenant, owner-scoped resource authorization.
//!
//! NEVER use these in production: no pooling, TLS, HA, session issuance,
//! key custody or production lifecycle. They refuse remote hosts and normal
//! PostgreSQL port, and require an explicit isolated test-cluster guard.
//! They neither mount an HTTP route nor permit mutations.

use crate::web::csrf::SessionCsrfToken;
use crate::web::session_preflight::{ResourceAuthorization, SessionLookup, TrustedSession};
use postgres::{Client, Config, NoTls};
use std::collections::HashMap;
use std::time::Duration;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum DisposablePgError {
    InvalidConfiguration,
    NotDisposable,
}

/// Credentials must be supplied by the trusted server/test harness, NEVER
/// from headers, URL parameters, form fields or an unverified JWT claim.
pub struct TenantDatabaseLogin {
    role: String,
    password: String,
}

impl TenantDatabaseLogin {
    pub fn new(role: &str, password: String) -> Self {
        Self {
            role: role.to_owned(),
            password,
        }
    }
}

fn guarded_port(host: &str, port: u16) -> Result<(), DisposablePgError> {
    if host != "127.0.0.1" || port == 0 || port == 5432 {
        return Err(DisposablePgError::InvalidConfiguration);
    }
    Ok(())
}

fn test_cluster_guard() -> Result<(), ()> {
    if std::env::var("NULANG_TEST_POSTGRES_DISPOSABLE").as_deref() != Ok("1") {
        return Err(());
    }
    let root = std::env::var("NULANG_TEST_POSTGRES_DATA_DIR").map_err(|_| ())?;
    if !root.starts_with("/tmp/nulang-web-session-pg.")
        || !root.ends_with("/data")
        || !std::path::Path::new(&root).is_dir()
    {
        return Err(());
    }
    Ok(())
}

fn connect(port: u16, role: &str, password: &str) -> Result<Client, ()> {
    test_cluster_guard()?;
    let mut config = Config::new();
    config
        .host("127.0.0.1")
        .port(port)
        .user(role)
        .password(password)
        .dbname("postgres")
        .connect_timeout(Duration::from_secs(3));
    let mut client = config.connect(NoTls).map_err(|_| ())?;

    let row = client
        .query_one(
            "SELECT inet_server_addr()::text, inet_server_port(), \
                    current_user::text, session_user::text, \
                    rolsuper, rolbypassrls \
               FROM pg_catalog.pg_roles WHERE rolname = current_user::name",
            &[],
        )
        .map_err(|_| ())?;
    let host: Option<String> = row.get(0);
    let connected_port: Option<i32> = row.get(1);
    let current_user: String = row.get(2);
    let session_user: String = row.get(3);
    let superuser: bool = row.get(4);
    let bypass_rls: bool = row.get(5);
    if host.as_deref() != Some("127.0.0.1")
        || connected_port != Some(i32::from(port))
        || current_user != role
        || session_user != role
        || superuser
        || bypass_rls
    {
        return Err(());
    }

    // The fixture owner inserts a fresh random nonce into this throwaway
    // cluster. The role needs SELECT on only the guard row, not elevated
    // pg_read_all_settings privileges.
    let actual: String = client
        .query_one("SELECT nonce FROM nulang_private.cluster_identity", &[])
        .map_err(|_| ())?
        .get(0);
    let expected = std::env::var("NULANG_TEST_POSTGRES_NONCE").map_err(|_| ())?;
    if actual != expected || expected.len() != 64 {
        return Err(());
    }
    Ok(client)
}

/// Read-only session store adapter. The database holds *BLAKE3-hashed*
/// high-entropy handles, not raw bearer cookie values. The session verifier
/// account is granted SELECT only on the private sessions table and no
/// tenant-inventory privileges.
pub struct DisposableSessionLookup {
    port: u16,
    role: String,
    password: String,
}

impl DisposableSessionLookup {
    pub fn new(
        host: &str,
        port: u16,
        role: &str,
        password: String,
    ) -> Result<Self, DisposablePgError> {
        guarded_port(host, port)?;
        if role.is_empty() || password.is_empty() {
            return Err(DisposablePgError::InvalidConfiguration);
        }
        Ok(Self {
            port,
            role: role.into(),
            password,
        })
    }
}

impl SessionLookup for DisposableSessionLookup {
    fn lookup(&self, opaque_cookie: &str) -> Result<Option<TrustedSession>, ()> {
        let mut client = connect(self.port, &self.role, &self.password)?;
        // Handles must be cryptographically random in real session issuance.
        // Test fixture uses deterministic handles ONLY inside disposable PG.
        let digest = blake3::hash(opaque_cookie.as_bytes()).to_hex().to_string();
        let rows = client
            .query(
                "SELECT principal_id, tenant_id, expires_at, csrf_hex \
                 FROM nulang_private.sessions \
                 WHERE handle_digest = $1 AND NOT revoked",
                &[&digest],
            )
            .map_err(|_| ())?;
        let Some(row) = rows.first() else {
            return Ok(None);
        };
        let expires: i64 = row.get(2);
        let csrf_hex: String = row.get(3);
        let csrf = SessionCsrfToken::from_server_storage_hex(&csrf_hex).map_err(|_| ())?;
        let expires = u64::try_from(expires).map_err(|_| ())?;
        Ok(Some(TrustedSession::new(
            row.get::<_, String>(0),
            row.get::<_, String>(1),
            expires,
            csrf,
        )))
    }
}

/// A separate, SELECT-only PostgreSQL login per tenant. RLS derives the
/// *effective* tenant from an owner-controlled login mapping, not request
/// variables or freely SETtable session GUCs.
pub struct DisposableTenantResourcePolicy {
    port: u16,
    tenants: HashMap<String, TenantDatabaseLogin>,
}

impl DisposableTenantResourcePolicy {
    pub fn new(
        host: &str,
        port: u16,
        tenants: HashMap<String, TenantDatabaseLogin>,
    ) -> Result<Self, DisposablePgError> {
        guarded_port(host, port)?;
        if tenants.is_empty()
            || tenants.iter().any(|(tenant, login)| {
                tenant.is_empty() || login.role.is_empty() || login.password.is_empty()
            })
        {
            return Err(DisposablePgError::InvalidConfiguration);
        }
        Ok(Self { port, tenants })
    }
}

impl ResourceAuthorization for DisposableTenantResourcePolicy {
    fn can_preflight(&self, principal: &str, tenant: &str, resource: &str) -> Result<bool, ()> {
        let Some(login) = self.tenants.get(tenant) else {
            return Ok(false);
        };
        let mut client = connect(self.port, &login.role, &login.password)?;

        // PostgreSQL mapping is a protected SECURITY DEFINER function
        // bound to session_user, not a client-provided tenant claim.
        let actual_tenant: Option<String> = client
            .query_one("SELECT nulang_private.current_tenant()", &[])
            .map_err(|_| ())?
            .get(0);
        if actual_tenant.as_deref() != Some(tenant) {
            return Err(());
        }

        let row = client
            .query_one(
                "SELECT EXISTS(SELECT 1 FROM nulang_example.orders \
                  WHERE id = $1 AND owner_id = $2)",
                &[&resource, &principal],
            )
            .map_err(|_| ())?;
        Ok(row.get(0))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn reference_rejects_remote_hosts_and_default_port() {
        assert!(matches!(
            DisposableSessionLookup::new("db.example", 15433, "reader", "test".into()),
            Err(DisposablePgError::InvalidConfiguration)
        ));
        assert!(matches!(
            DisposableSessionLookup::new("127.0.0.1", 5432, "reader", "test".into()),
            Err(DisposablePgError::InvalidConfiguration)
        ));
        assert!(matches!(
            DisposableSessionLookup::new("127.0.0.1", 15433, "", "test".into()),
            Err(DisposablePgError::InvalidConfiguration)
        ));
        assert!(matches!(
            DisposableTenantResourcePolicy::new("127.0.0.1", 15433, HashMap::new()),
            Err(DisposablePgError::InvalidConfiguration)
        ));
    }

    #[test]
    fn reference_adapters_fail_without_disposable_cluster_guard() {
        // No database connection is attempted when no server-owned fixture
        // guard has been configured.
        if std::env::var("NULANG_TEST_POSTGRES_DISPOSABLE").as_deref() != Ok("1") {
            let lookup =
                DisposableSessionLookup::new("127.0.0.1", 15433, "reader", "test".into()).unwrap();
            assert!(lookup.lookup("nonexistent").is_err());
        }
    }
}
