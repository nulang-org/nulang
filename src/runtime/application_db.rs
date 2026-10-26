//! Application query execution, separate from the actor durability store.
//!
//! This boundary is deliberately opt-in: no application query can implicitly
//! run with the credentials used to checkpoint actors and workflow journals.
//! Provision the application database with a separate, least-privilege role.

use super::Runtime;
use crate::vm::Value;
use std::io;

/// Deliberately separate from PersistenceStore. Callers must configure an
/// application executor explicitly, ideally with a different DB user/schema.
pub trait ApplicationDatabase: Send + Sync {
    /// Each row must be a JSON array. Errors are surfaced to the caller.
    fn query(&self, sql: &str, params: &[Value]) -> io::Result<Vec<String>>;
}

impl Runtime {
    /// Configure an explicitly scoped application query executor. Never
    /// defaults to or wraps the actor durability store.
    pub fn set_application_database(&mut self, database: Box<dyn ApplicationDatabase>) {
        self.application_database = Some(database);
    }

    /// Execute application SQL or fail closed if no executor is configured.
    pub fn query_application_rows(&self, sql: &str, params: &[Value]) -> io::Result<Vec<String>> {
        let database = self.application_database.as_ref().ok_or_else(|| {
            io::Error::new(
                io::ErrorKind::NotConnected,
                "application database is not configured",
            )
        })?;
        database.query(sql, params)
    }

    /// Explicit result protocol for DB.query_result: success rows or typed
    /// error code and message.
    ///
    /// Untrusted backend error strings are never returned to source programs:
    /// these strings can contain raw SQL, credentials and private row values.
    pub fn query_application_result_json(&self, sql: &str, params: &[Value]) -> String {
        if self.application_database.is_none() {
            return query_error_json(
                "DATABASE_NOT_CONFIGURED",
                "application database is not configured",
            );
        }
        let rows = match self.query_application_rows(sql, params) {
            Ok(rows) => rows,
            Err(_) => return query_error_json("QUERY_FAILED", "application database query failed"),
        };
        let mut parsed = Vec::with_capacity(rows.len());
        for row in rows {
            match serde_json::from_str::<serde_json::Value>(&row) {
                Ok(serde_json::Value::Array(values)) => {
                    parsed.push(serde_json::Value::Array(values))
                }
                _ => {
                    return query_error_json(
                        "INVALID_ROW_ENCODING",
                        "application database returned an invalid row",
                    )
                }
            }
        }
        serde_json::json!({"ok": true, "rows": parsed}).to_string()
    }
}

pub(crate) fn query_error_json(code: &str, message: &str) -> String {
    serde_json::json!({"ok": false, "error": {"code": code, "message": message}}).to_string()
}

/// Route legacy DB.query and the opt-in DB.query_result through the *same*
/// application-only executor, never through Runtime.persistence.
/// Legacy DB.query preserves its historical string-of-JSON-rows response and
/// nil-on-error contract; DB.query_result is the safe structured alternative.
pub(crate) fn dispatch_application_query(
    runtime: &Runtime,
    operation: &str,
    constants: &[crate::bytecode::Constant],
    args: &[Value],
) -> Option<String> {
    if operation != "query" && operation != "query_result" {
        return None;
    }
    let sql = args
        .first()
        .and_then(|value| value.as_string_id())
        .and_then(|index| constants.get(index as usize))
        .and_then(|constant| match constant {
            crate::bytecode::Constant::String(sql) if !sql.trim().is_empty() => Some(sql.as_str()),
            _ => None,
        });
    let sql = match sql {
        Some(sql) => sql,
        None if operation == "query_result" => {
            return Some(query_error_json(
                "INVALID_SQL_ARGUMENT",
                "SQL must be a nonempty string literal",
            ))
        }
        None => return None,
    };
    let params = &args[1..];
    if operation == "query_result" {
        return Some(runtime.query_application_result_json(sql, params));
    }
    // Maintain compatibility with the old representation while preventing
    // implicit access to internal actor snapshot/journal tables.
    runtime
        .query_application_rows(sql, params)
        .ok()
        .and_then(|rows| serde_json::to_string(&rows).ok())
}

/// Thin PostgreSQL adapter. Unlike PostgresStore, this does not create
/// internal actor tables and owns an entirely separate connection.
#[cfg(feature = "postgres")]
pub struct PostgresApplicationDatabase {
    client: std::sync::Mutex<postgres::Client>,
}

#[cfg(feature = "postgres")]
impl PostgresApplicationDatabase {
    pub fn connect(connection_string: &str) -> io::Result<Self> {
        use postgres::config::Host;
        let config: postgres::Config = connection_string.parse().map_err(|_| {
            io::Error::new(
                io::ErrorKind::InvalidInput,
                "invalid application PostgreSQL configuration",
            )
        })?;
        // NoTls is acceptable only over Unix sockets / explicitly local TCP.
        // Avoid passing credentials to a remote server in plaintext.
        let local_only = !config.get_hosts().is_empty()
            && config.get_hosts().iter().all(|host| match host {
                Host::Tcp(host) => matches!(host.as_str(), "localhost" | "127.0.0.1" | "::1"),
                #[cfg(unix)]
                Host::Unix(_) => true,
            });
        if !local_only {
            return Err(io::Error::new(
                io::ErrorKind::PermissionDenied,
                "NoTls application PostgreSQL requires an explicit local host or Unix socket",
            ));
        }
        let client = config.connect(postgres::NoTls).map_err(|_| {
            io::Error::new(
                io::ErrorKind::ConnectionRefused,
                "application PostgreSQL connection failed",
            )
        })?;
        Ok(Self {
            client: std::sync::Mutex::new(client),
        })
    }
}

#[cfg(feature = "postgres")]
impl ApplicationDatabase for PostgresApplicationDatabase {
    fn query(&self, sql: &str, params: &[Value]) -> io::Result<Vec<String>> {
        let mut client = self
            .client
            .lock()
            .map_err(|_| io::Error::other("application PostgreSQL connection poisoned"))?;
        let typed_params: Vec<Box<dyn postgres::types::ToSql + Sync>> = params
            .iter()
            .map(|value| {
                if value.is_nil() {
                    Box::new(None::<String>) as Box<dyn postgres::types::ToSql + Sync>
                } else if let Some(integer) = value.as_int() {
                    Box::new(integer) as Box<dyn postgres::types::ToSql + Sync>
                } else if let Some(float) = value.as_float() {
                    Box::new(float) as Box<dyn postgres::types::ToSql + Sync>
                } else if let Some(boolean) = value.as_bool() {
                    Box::new(boolean) as Box<dyn postgres::types::ToSql + Sync>
                } else {
                    Box::new(value.to_string_repr()) as Box<dyn postgres::types::ToSql + Sync>
                }
            })
            .collect();
        let bindings: Vec<&(dyn postgres::types::ToSql + Sync)> =
            typed_params.iter().map(|value| value.as_ref()).collect();
        let rows = client.query(sql, &bindings).map_err(io::Error::other)?;
        rows.iter()
            .map(super::persistence::postgres_row_json)
            .collect()
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::runtime::Runtime;
    use std::io;
    use std::sync::atomic::{AtomicUsize, Ordering};
    use std::sync::Arc;

    struct FakeApplicationDatabase {
        calls: Arc<AtomicUsize>,
        result: io::Result<Vec<String>>,
    }

    impl ApplicationDatabase for FakeApplicationDatabase {
        fn query(&self, _sql: &str, _params: &[crate::vm::Value]) -> io::Result<Vec<String>> {
            self.calls.fetch_add(1, Ordering::SeqCst);
            match &self.result {
                Ok(rows) => Ok(rows.clone()),
                Err(error) => Err(io::Error::new(error.kind(), error.to_string())),
            }
        }
    }

    #[test]
    fn no_application_database_fails_closed_even_when_durability_exists() {
        let runtime = Runtime::new();
        let result = runtime.query_application_result_json("SELECT 1", &[]);
        let json: serde_json::Value = serde_json::from_str(&result).unwrap();
        assert_eq!(json["ok"], false);
        assert_eq!(json["error"]["code"], "DATABASE_NOT_CONFIGURED");
    }

    #[test]
    fn application_queries_use_explicit_executor_not_durability_store() {
        let calls = Arc::new(AtomicUsize::new(0));
        let mut runtime = Runtime::new();
        runtime.set_application_database(Box::new(FakeApplicationDatabase {
            calls: calls.clone(),
            result: Ok(vec!["[42,null,true]".to_owned()]),
        }));
        let result: serde_json::Value =
            serde_json::from_str(&runtime.query_application_result_json("SELECT 42", &[])).unwrap();
        assert_eq!(result["ok"], true);
        assert_eq!(result["rows"], serde_json::json!([[42, null, true]]));
        assert_eq!(calls.load(Ordering::SeqCst), 1);
        // The default durability store does not support arbitrary SQL.
        assert!(runtime.persistence.query("SELECT 42", &[]).is_err());
    }

    #[test]
    fn checked_dispatch_requires_sql_literal_and_never_returns_nil() {
        let runtime = Runtime::new();
        let args = [Value::string(0)];
        let constants = [crate::bytecode::Constant::String("SELECT 1".to_owned())];
        let value = dispatch_application_query(&runtime, "query_result", &constants, &args)
            .expect("checked operation must return a result");
        let decoded: serde_json::Value = serde_json::from_str(&value).unwrap();
        assert_eq!(decoded["error"]["code"], "DATABASE_NOT_CONFIGURED");

        let malformed = dispatch_application_query(&runtime, "query_result", &[], &args)
            .expect("malformed SQL must return an error object");
        let decoded: serde_json::Value = serde_json::from_str(&malformed).unwrap();
        assert_eq!(decoded["error"]["code"], "INVALID_SQL_ARGUMENT");
        assert!(dispatch_application_query(&runtime, "query", &[], &args).is_none());
    }

    #[test]
    fn legacy_dispatch_can_only_use_explicit_application_executor() {
        let mut runtime = Runtime::new();
        let args = [Value::string(0)];
        let constants = [crate::bytecode::Constant::String("SELECT 1".to_owned())];
        assert!(dispatch_application_query(&runtime, "query", &constants, &args).is_none());
        runtime.set_application_database(Box::new(FakeApplicationDatabase {
            calls: Arc::new(AtomicUsize::new(0)),
            result: Ok(vec!["[1]".to_string()]),
        }));
        assert_eq!(
            dispatch_application_query(&runtime, "query", &constants, &args).unwrap(),
            r#"["[1]"]"#
        );
    }

    #[test]
    fn backend_errors_are_structured_and_do_not_expose_sql_or_credentials() {
        let mut runtime = Runtime::new();
        runtime.set_application_database(Box::new(FakeApplicationDatabase {
            calls: Arc::new(AtomicUsize::new(0)),
            result: Err(io::Error::other(
                "postgres://password:secret@db/private SELECT token FROM accounts",
            )),
        }));
        let raw = runtime.query_application_result_json("SELECT token FROM accounts", &[]);
        let result: serde_json::Value = serde_json::from_str(&raw).unwrap();
        assert_eq!(result["ok"], false);
        assert_eq!(result["error"]["code"], "QUERY_FAILED");
        assert!(!raw.contains("secret"));
        assert!(!raw.contains("token FROM"));
    }

    #[cfg(feature = "postgres")]
    #[test]
    fn postgres_application_connection_refuses_unencrypted_remote_hosts() {
        let error = PostgresApplicationDatabase::connect(
            "host=remote.database.internal user=app password=secret dbname=app",
        )
        .err()
        .expect("should reject remote NoTls before opening socket");
        assert_eq!(error.kind(), io::ErrorKind::PermissionDenied);
        assert!(!error.to_string().contains("secret"));
    }

    #[cfg(feature = "postgres")]
    #[test]
    fn postgres_application_backend_returns_typed_rows_without_internal_schema_setup() {
        let Ok(url) = std::env::var("NU_TEST_APP_POSTGRES_URL") else {
            return;
        };
        let app = PostgresApplicationDatabase::connect(&url).unwrap();
        let rows = app
            .query(
                "SELECT 42::int4, NULL::text, decode('00ff', 'hex')::bytea",
                &[],
            )
            .unwrap();
        assert_eq!(
            serde_json::from_str::<serde_json::Value>(&rows[0]).unwrap(),
            serde_json::json!([42, null, [0, 255]])
        );
    }

    #[test]
    fn invalid_driver_rows_are_not_silently_discarded() {
        let mut runtime = Runtime::new();
        runtime.set_application_database(Box::new(FakeApplicationDatabase {
            calls: Arc::new(AtomicUsize::new(0)),
            result: Ok(vec!["this is not JSON".to_owned()]),
        }));
        let result: serde_json::Value =
            serde_json::from_str(&runtime.query_application_result_json("SELECT 1", &[])).unwrap();
        assert_eq!(result["ok"], false);
        assert_eq!(result["error"]["code"], "INVALID_ROW_ENCODING");
    }
}
