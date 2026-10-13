//! A checked, serializable result for the optional `DB.query_checked` effect.
//!
//! Existing `DB.query` remains unchanged. The checked variant currently
//! returns a JSON string envelope, not a native Nulang `Result<T, DbError>`.
//! This is intentionally a migration bridge while typed SQL and nonblocking
//! database effects are developed. Callers must not expose raw database
//! diagnostics to untrusted clients.

use std::io;

use serde::Serialize;

/// A machine-readable, backend-independent category for query failures.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum DbQueryErrorCode {
    Unsupported,
    InvalidInput,
    DecodeFailure,
    QueryFailed,
    PermissionDenied,
}

/// Intentionally sanitized: internal SQL errors can contain sensitive values.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct DbQueryError {
    pub code: DbQueryErrorCode,
    pub message: &'static str,
}

#[derive(Debug, Clone, PartialEq, Serialize)]
#[serde(tag = "status", rename_all = "snake_case")]
pub enum DbQueryOutcome {
    Ok { rows: Vec<Vec<serde_json::Value>> },
    Error { error: DbQueryError },
}

impl DbQueryOutcome {
    pub fn invalid_query_argument() -> Self {
        Self::Error {
            error: DbQueryError {
                code: DbQueryErrorCode::InvalidInput,
                message: "A SQL query string is required.",
            },
        }
    }

    fn error(code: DbQueryErrorCode) -> Self {
        let message = match code {
            DbQueryErrorCode::Unsupported => "Database queries are unavailable on this backend.",
            DbQueryErrorCode::InvalidInput => "Invalid database query parameters.",
            DbQueryErrorCode::DecodeFailure => "Database returned an invalid row.",
            DbQueryErrorCode::QueryFailed => "Database query failed; check server logs.",
            DbQueryErrorCode::PermissionDenied => "Database query permission denied.",
        };
        Self::Error {
            error: DbQueryError { code, message },
        }
    }

    /// Converts the existing persistence-query row format into actual JSON
    /// arrays; invalid/malformed backend rows fail rather than disappearing.
    pub fn from_legacy_rows(result: io::Result<Vec<String>>) -> Self {
        match result {
            Ok(encoded_rows) => {
                let rows = encoded_rows
                    .iter()
                    .map(|encoded| serde_json::from_str::<Vec<serde_json::Value>>(encoded))
                    .collect::<Result<Vec<_>, _>>();
                match rows {
                    Ok(rows) => Self::Ok { rows },
                    Err(_) => Self::error(DbQueryErrorCode::DecodeFailure),
                }
            }
            Err(error) => {
                let code = match error.kind() {
                    io::ErrorKind::Unsupported => DbQueryErrorCode::Unsupported,
                    io::ErrorKind::InvalidInput => DbQueryErrorCode::InvalidInput,
                    io::ErrorKind::InvalidData => DbQueryErrorCode::DecodeFailure,
                    io::ErrorKind::PermissionDenied => DbQueryErrorCode::PermissionDenied,
                    _ => DbQueryErrorCode::QueryFailed,
                };
                Self::error(code)
            }
        }
    }

    pub fn code(&self) -> Option<DbQueryErrorCode> {
        match self {
            Self::Ok { .. } => None,
            Self::Error { error } => Some(error.code),
        }
    }

    pub fn to_json(&self) -> String {
        // The static envelope and JSON value types are always serializable.
        serde_json::to_string(self).expect("DbQueryOutcome must serialize")
    }
}

//! Stable, serializable envelope for the opt-in DB.query_checked effect.
//!
//! This is a compatibility bridge: the VM currently returns heap strings and
//! persistence backends return JSON-encoded rows. Future typed/async database
//! packages can consume the same error codes without changing DB.query.

#[cfg(test)]
mod tests {
    use super::*;
    use std::io;

    #[test]
    fn checked_query_distinguishes_empty_success_from_error() {
        let empty = DbQueryOutcome::from_legacy_rows(Ok(vec![]));
        assert_eq!(empty.to_json(), r#"{"status":"ok","rows":[]}"#);

        let failed = DbQueryOutcome::from_legacy_rows(Err(io::Error::new(
            io::ErrorKind::Unsupported,
            "DB backend not available",
        )));
        assert_eq!(
            failed.to_json(),
            r#"{"status":"error","error":{"code":"unsupported","message":"Database queries are unavailable on this backend."}}"#
        );
    }

    #[test]
    fn checked_query_parses_nested_rows_and_nulls() {
        let result = DbQueryOutcome::from_legacy_rows(Ok(vec![
            r#"[1,null,{"items":[2,3]}]"#.into(),
            r#"["hello",true]"#.into(),
        ]));
        assert_eq!(
            result.to_json(),
            r#"{"status":"ok","rows":[[1,null,{"items":[2,3]}],["hello",true]]}"#
        );
    }

    #[test]
    fn malformed_backend_row_is_a_decoding_error() {
        let result = DbQueryOutcome::from_legacy_rows(Ok(vec![
            r#"[1,2]"#.into(),
            r#"{"not":"a row"}"#.into(),
        ]));
        assert_eq!(result.code(), Some(DbQueryErrorCode::DecodeFailure));
    }

    #[test]
    fn checked_query_classifies_invalid_input_and_backend_failure() {
        let invalid = DbQueryOutcome::invalid_query_argument();
        assert_eq!(invalid.code(), Some(DbQueryErrorCode::InvalidInput));

        let failed = DbQueryOutcome::from_legacy_rows(Err(io::Error::other("syntax error")));
        assert_eq!(failed.code(), Some(DbQueryErrorCode::QueryFailed));
        assert!(!failed.to_json().contains("syntax error"));
    }
}
