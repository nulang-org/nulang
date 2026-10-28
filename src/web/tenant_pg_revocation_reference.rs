//! Local disposable PostgreSQL revocation lookup for pinned JWT acceptance.
//! NOT a production auth service: this assumes a privileged setup harness
//! and permits only local pg_virtualenv with a distinct SELECT-only login.
//!
//! The contract returns None on every storage, authorization or connection
//! error; the JWT verifier treats None exactly like revoked.
use crate::web::tenant_jwt_reference::RevocationPolicy;
use postgres::{Config, NoTls};
use std::time::Duration;

/// Credentials are server-owned and must never be serialized or logged.
/// Each verification makes a fresh read (no revocation cache).
pub struct DisposablePostgresRevocation {
    host: String,
    port: u16,
    user: String,
    password: String,
}

impl DisposablePostgresRevocation {
    pub fn new(host: &str, port: u16, user: &str, password: String) -> Option<Self> {
        if host != "127.0.0.1"
            || port == 0
            || port == 5432
            || user != "example_token_verifier"
            || password.is_empty()
        {
            return None;
        }
        Some(Self {
            host: host.to_string(),
            port,
            user: user.to_string(),
            password,
        })
    }

    fn disposable_guard(&self) -> Option<()> {
        if std::env::var("NULANG_TEST_POSTGRES_DISPOSABLE")
            .ok()?
            .as_str()
            != "1"
        {
            return None;
        }
        let root = std::env::var("PG_CLUSTER_CONF_ROOT").ok()?;
        if !root.starts_with("/tmp/pg_virtualenv.") || !root.ends_with("/postgresql") {
            return None;
        }
        Some(())
    }
}

impl RevocationPolicy for DisposablePostgresRevocation {
    fn is_revoked(&self, issuer: &str, subject: &str, jti: &str) -> Option<bool> {
        self.disposable_guard()?;
        if issuer.is_empty() || subject.is_empty() || jti.is_empty() {
            return None;
        }
        let mut config = Config::new();
        config
            .host(&self.host)
            .port(self.port)
            .user(&self.user)
            .password(&self.password)
            .dbname("postgres")
            .connect_timeout(Duration::from_secs(2));
        let mut connection = config.connect(NoTls).ok()?;
        let row = connection
            .query_one(
                "SELECT pg_catalog.host(inet_server_addr()), inet_server_port(), \
                    session_user::text, current_user::text, \
                    rolsuper, rolbypassrls \
                 FROM pg_catalog.pg_roles WHERE rolname = session_user::name",
                &[],
            )
            .ok()?;
        let addr: Option<String> = row.get(0);
        let port: Option<i32> = row.get(1);
        let session_user: String = row.get(2);
        let current_user: String = row.get(3);
        let superuser: bool = row.get(4);
        let bypass_rls: bool = row.get(5);
        if addr.as_deref() != Some("127.0.0.1")
            || port != Some(i32::from(self.port))
            || session_user != self.user
            || current_user != self.user
            || superuser
            || bypass_rls
        {
            return None;
        }
        let result = connection
            .query_one(
                "SELECT EXISTS (SELECT 1 \
                 FROM tenant_example_private.revoked_tokens \
                 WHERE issuer = $1 AND subject = $2 AND jti = $3)",
                &[&issuer, &subject, &jti],
            )
            .ok()?;
        Some(result.get(0))
    }
}
