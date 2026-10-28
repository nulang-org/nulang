//! Separate, INSERT-only revocation writer for the **disposable** reference.
//!
//! NOT a public endpoint or production admin service. The associated role
//! cannot read, update or delete revocations, nor access tenant inventory.
//! NoTls and the local pg_virtualenv guard forbid production connectivity.
use postgres::{Config, NoTls};
use std::time::Duration;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum RevocationWriteError {
    InvalidInput,
    Unavailable,
}

/// Owned credentials for the private disposable revocation writer role.
/// This intentionally has no Debug or Serialize implementation.
pub struct DisposablePostgresRevocationWriter {
    host: String,
    port: u16,
    user: String,
    password: String,
}

impl DisposablePostgresRevocationWriter {
    pub fn new(host: &str, port: u16, user: &str, password: String) -> Option<Self> {
        if host != "127.0.0.1"
            || port == 0
            || port == 5432
            || user != "example_token_revoker"
            || password.is_empty()
        {
            return None;
        }
        Some(Self {
            host: host.into(),
            port,
            user: user.into(),
            password,
        })
    }

    /// Only a privileged, server-owned caller with the fixture writer secret
    /// may revoke a token. Duplicate calls are idempotent via the primary key.
    /// Never expose this method to an unauthenticated HTTP route.
    pub fn revoke(
        &self,
        issuer: &str,
        subject: &str,
        jti: &str,
    ) -> Result<(), RevocationWriteError> {
        if issuer.is_empty()
            || issuer.len() > 1024
            || subject.is_empty()
            || subject.len() > 256
            || subject.contains('|')
            || jti.is_empty()
            || jti.len() > 256
            || [issuer, subject, jti]
                .iter()
                .any(|s| s.chars().any(char::is_control))
        {
            return Err(RevocationWriteError::InvalidInput);
        }

        let config_root =
            std::env::var("PG_CLUSTER_CONF_ROOT").map_err(|_| RevocationWriteError::Unavailable)?;
        if std::env::var("NULANG_TEST_POSTGRES_DISPOSABLE").as_deref() != Ok("1")
            || !config_root.starts_with("/tmp/pg_virtualenv.")
            || !config_root.ends_with("/postgresql")
        {
            return Err(RevocationWriteError::Unavailable);
        }

        let mut config = Config::new();
        config
            .host(&self.host)
            .port(self.port)
            .user(&self.user)
            .password(&self.password)
            .dbname("postgres")
            .connect_timeout(Duration::from_secs(2));
        let mut client = config
            .connect(NoTls)
            .map_err(|_| RevocationWriteError::Unavailable)?;
        let identity = client
            .query_one(
                "SELECT pg_catalog.host(inet_server_addr()), inet_server_port(), \
                 session_user::text, current_user::text, rolsuper, rolbypassrls \
                 FROM pg_catalog.pg_roles WHERE rolname = session_user::name",
                &[],
            )
            .map_err(|_| RevocationWriteError::Unavailable)?;
        let addr: Option<String> = identity.get(0);
        let port: Option<i32> = identity.get(1);
        let session_user: String = identity.get(2);
        let current_user: String = identity.get(3);
        let superuser: bool = identity.get(4);
        let bypass_rls: bool = identity.get(5);
        if addr.as_deref() != Some("127.0.0.1")
            || port != Some(i32::from(self.port))
            || session_user != self.user
            || current_user != self.user
            || superuser
            || bypass_rls
        {
            return Err(RevocationWriteError::Unavailable);
        }
        client
            .execute(
                "INSERT INTO tenant_example_private.revoked_tokens (issuer, subject, jti) \
                 VALUES ($1, $2, $3) ON CONFLICT DO NOTHING",
                &[&issuer, &subject, &jti],
            )
            .map_err(|_| RevocationWriteError::Unavailable)?;
        Ok(())
    }
}
