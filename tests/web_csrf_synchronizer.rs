//! The CSRF synchronizer protects session-authenticated unsafe browser requests.
//! Integration of an HTTP endpoint remains deliberately out of scope.
use nulang::web::csrf::{CsrfTokenError, SessionCsrfToken};

#[test]
fn server_stored_csrf_hex_round_trips_and_rejects_corrupt_state() {
    let original = SessionCsrfToken::generate().unwrap();
    let saved = original.encode();
    let recovered = SessionCsrfToken::from_server_storage_hex(&saved).unwrap();
    assert_eq!(recovered.verify_presented(&[saved.as_str()]), Ok(()));
    assert_eq!(
        SessionCsrfToken::from_server_storage_hex("invalid").unwrap_err(),
        CsrfTokenError::Malformed
    );
    assert_eq!(
        SessionCsrfToken::from_server_storage_hex(&saved.to_ascii_uppercase()).err(),
        if saved != saved.to_ascii_uppercase() {
            Some(CsrfTokenError::Malformed)
        } else {
            None
        }
    );
}

#[test]
fn random_token_is_canonical_and_verifies_only_for_own_session() {
    let session_a = SessionCsrfToken::generate().unwrap();
    let session_b = SessionCsrfToken::generate().unwrap();
    let presented = session_a.encode();

    assert_eq!(presented.len(), 64);
    assert!(presented
        .bytes()
        .all(|c| c.is_ascii_digit() || (b'a'..=b'f').contains(&c)));
    assert_ne!(presented, session_b.encode());
    assert_eq!(session_a.verify_presented(&[presented.as_str()]), Ok(()));
    assert_eq!(
        session_b.verify_presented(&[presented.as_str()]),
        Err(CsrfTokenError::Mismatch),
        "valid tokens from different sessions must not be interchangeable"
    );
}

#[test]
fn missing_and_ambiguous_tokens_are_rejected_even_if_they_match() {
    let session = SessionCsrfToken::generate().unwrap();
    let token = session.encode();

    assert_eq!(session.verify_presented(&[]), Err(CsrfTokenError::Missing));
    assert_eq!(
        session.verify_presented(&[""]),
        Err(CsrfTokenError::Malformed)
    );
    assert_eq!(
        session.verify_presented(&[token.as_str(), token.as_str()]),
        Err(CsrfTokenError::Ambiguous),
        "header and form token duplication must not silently resolve"
    );
}

#[test]
fn wrong_length_uppercase_whitespace_and_invalid_hex_fail_closed() {
    let session = SessionCsrfToken::generate().unwrap();
    let token = session.encode();
    let uppercase = token.to_ascii_uppercase();
    // Uppercase forms must be refused regardless of whether the random token
    // happens to contain letters. Ensure one specific hexadecimal letter.
    let mut deliberately_uppercase = token.clone();
    deliberately_uppercase.replace_range(..1, "A");

    for candidate in [
        "short".to_owned(),
        format!(" {token}"),
        format!("{token} "),
        format!("{token}\u{0000}"),
        deliberately_uppercase,
        "g".repeat(64),
        "0".repeat(65),
    ] {
        assert_eq!(
            session.verify_presented(&[candidate.as_str()]),
            Err(CsrfTokenError::Malformed),
            "must reject malformed token framing"
        );
    }
    if uppercase != token {
        assert_eq!(
            session.verify_presented(&[uppercase.as_str()]),
            Err(CsrfTokenError::Malformed)
        );
    }
}

#[test]
fn csrf_tokens_are_redacted_in_debug_and_do_not_serialize_by_default() {
    let session = SessionCsrfToken::generate().unwrap();
    let encoded = session.encode();
    let debug = format!("{session:?}");
    assert!(!debug.contains(&encoded));
    assert!(debug.contains("redacted"));
}

#[test]
fn valid_and_invalid_tokens_never_change_preflight_functionality() {
    // This module intentionally exposes a token primitive, not a server-side
    // authentication grant or a public preflight HTTP endpoint.
    let session = SessionCsrfToken::generate().unwrap();
    let other = SessionCsrfToken::generate().unwrap();
    assert_eq!(
        session.verify_presented(&[other.encode().as_str()]),
        Err(CsrfTokenError::Mismatch)
    );
    assert!(session
        .verify_presented(&[session.encode().as_str()])
        .is_ok());
}

#[test]
fn csrf_checked_preflight_rejects_before_decoding_and_then_reports_fields() {
    use nulang::web::bindings::{RouteBindingContract, RouteBindingSource};
    use nulang::web::csrf::{preflight_after_session_csrf, CsrfPreflightError};
    use nulang::web::http_request::HttpRequestBindingInputs;
    use nulang::web::request_bindings::RequestDecodeErrorKind;
    use std::collections::HashMap;

    let session = SessionCsrfToken::generate().unwrap();
    let valid = session.encode();
    let path = HashMap::new();
    let headers = vec![(
        "Content-Type".to_owned(),
        "application/x-www-form-urlencoded".to_owned(),
    )];
    let captured = HttpRequestBindingInputs::capture("/orders", &headers, b"quantity=bad");
    let values = captured.values(&path, &headers);
    let binding = RouteBindingContract {
        source: RouteBindingSource::Form,
        source_name: "quantity".to_string(),
        handler_param: "internal_quantity_slot".to_string(),
        handler_index: 0,
        ty: Some("Int".to_string()),
    };

    let denied =
        preflight_after_session_csrf(&session, &[], &[binding.clone()], 1, &values).unwrap_err();
    assert!(matches!(
        denied,
        CsrfPreflightError::Csrf(CsrfTokenError::Missing)
    ));

    // Even a malformed compiler plan must not be inspected until the token
    // has passed the caller's session-bound gate.
    let denied =
        preflight_after_session_csrf(&session, &[], &[binding.clone()], 2, &values).unwrap_err();
    assert!(matches!(
        denied,
        CsrfPreflightError::Csrf(CsrfTokenError::Missing)
    ));

    let internal_error =
        preflight_after_session_csrf(&session, &[valid.as_str()], &[binding.clone()], 2, &values)
            .unwrap_err();
    assert!(matches!(internal_error, CsrfPreflightError::InvalidPlan(_)));

    let preflight =
        preflight_after_session_csrf(&session, &[valid.as_str()], &[binding], 1, &values).unwrap();
    assert_eq!(preflight.errors.len(), 1);
    assert_eq!(
        preflight.errors[0].kind,
        RequestDecodeErrorKind::InvalidType
    );
    assert_eq!(preflight.to_json()["errors"][0]["source_name"], "quantity");
}
