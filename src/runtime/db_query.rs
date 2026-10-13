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
