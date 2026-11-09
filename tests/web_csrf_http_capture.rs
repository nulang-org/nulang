use nulang::web::csrf::{CsrfTokenError, SessionCsrfToken};
use nulang::web::http_request::{capture_submitted_csrf_tokens, CsrfCaptureError};

fn form_headers() -> Vec<(String, String)> {
    vec![(
        "Content-Type".to_string(),
        "application/x-www-form-urlencoded; charset=UTF-8".to_string(),
    )]
}

#[test]
fn unique_form_token_and_case_insensitive_header_are_preserved() {
    let session = SessionCsrfToken::generate().unwrap();
    let token = session.encode();
    let raw = format!("title=hello&__nulang_csrf={token}");
    let captured = capture_submitted_csrf_tokens(&form_headers(), raw.as_bytes()).unwrap();
    assert_eq!(captured, vec![token.clone()]);
    assert!(session
        .verify_presented(&captured.iter().map(String::as_str).collect::<Vec<_>>())
        .is_ok());

    let captured =
        capture_submitted_csrf_tokens(&[("x-CsRf-ToKeN".into(), token.clone())], b"").unwrap();
    assert_eq!(captured, vec![token]);
}

#[test]
fn duplicate_form_keys_including_percent_encoded_aliases_are_preserved() {
    let session = SessionCsrfToken::generate().unwrap();
    let token = session.encode();
    let body = format!("__nulang_csrf={token}&__nulang_%63srf={token}");
    let captured = capture_submitted_csrf_tokens(&form_headers(), body.as_bytes()).unwrap();
    assert_eq!(captured.len(), 2);
    assert_eq!(
        session.verify_presented(&captured.iter().map(String::as_str).collect::<Vec<_>>()),
        Err(CsrfTokenError::Ambiguous)
    );
}

#[test]
fn header_and_form_tokens_together_or_duplicate_headers_are_ambiguous() {
    let session = SessionCsrfToken::generate().unwrap();
    let token = session.encode();
    let mut headers = form_headers();
    headers.push(("X-CSRF-Token".into(), token.clone()));
    let body = format!("__nulang_csrf={token}");
    let captured = capture_submitted_csrf_tokens(&headers, body.as_bytes()).unwrap();
    assert_eq!(captured.len(), 2);
    assert_eq!(
        session.verify_presented(&captured.iter().map(String::as_str).collect::<Vec<_>>()),
        Err(CsrfTokenError::Ambiguous)
    );

    let two_headers = vec![
        ("X-CSRF-Token".into(), token.clone()),
        ("x-csrf-token".into(), token.clone()),
    ];
    let captured = capture_submitted_csrf_tokens(&two_headers, b"").unwrap();
    assert_eq!(captured.len(), 2);
}

#[test]
fn json_and_query_strings_are_not_csrf_sources() {
    let session = SessionCsrfToken::generate().unwrap();
    let token = session.encode();
    assert!(capture_submitted_csrf_tokens(
        &[("Content-Type".into(), "application/json".into())],
        format!("{{\"__nulang_csrf\":\"{token}\"}}").as_bytes(),
    )
    .unwrap()
    .is_empty());

    assert!(capture_submitted_csrf_tokens(&[], b"").unwrap().is_empty());
}

#[test]
fn ambiguous_content_type_and_oversized_body_fail_closed() {
    let headers = vec![
        (
            "Content-Type".into(),
            "application/x-www-form-urlencoded".into(),
        ),
        (
            "content-type".into(),
            "application/x-www-form-urlencoded".into(),
        ),
    ];
    assert_eq!(
        capture_submitted_csrf_tokens(&headers, b"__nulang_csrf=hello"),
        Err(CsrfCaptureError::AmbiguousContentType)
    );

    assert_eq!(
        capture_submitted_csrf_tokens(&form_headers(), &vec![b'a'; 1024 * 1024 + 1]),
        Err(CsrfCaptureError::BodyTooLarge)
    );
    assert_eq!(
        capture_submitted_csrf_tokens(&form_headers(), b"__nulang_csrf=abc&bad=\xff"),
        Err(CsrfCaptureError::MalformedForm)
    );
}
