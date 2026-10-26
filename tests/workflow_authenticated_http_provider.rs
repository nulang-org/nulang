//! Authenticated provider configuration must reject insecure/unapproved URLs
//! *before* any network operation. Loopback HTTP is only a unit-test hook.
#![cfg(feature = "ureq")]
use nulang::workflow_http_provider::AuthenticatedWorkflowHttpProvider;

#[test]
fn only_explicitly_approved_https_endpoint_is_accepted() {
    let good = AuthenticatedWorkflowHttpProvider::new(
        "https://events.example.com/v1/accept",
        "events.example.com",
        "webhook:billing",
        "test-token.with-dots",
    );
    assert!(good.is_ok());

    for (endpoint, host) in [
        ("http://events.example.com/v1/accept", "events.example.com"),
        ("https://127.0.0.1/v1/accept", "127.0.0.1"),
        (
            "https://events.example.com.evil.test/v1/accept",
            "events.example.com",
        ),
        (
            "https://user@events.example.com/v1/accept",
            "events.example.com",
        ),
        (
            "https://events.example.com:4443/v1/accept",
            "events.example.com",
        ),
        (
            "https://events.example.com/v1/accept#part",
            "events.example.com",
        ),
        (
            "https://events.example.com/v1/accept?x=1",
            "events.example.com",
        ),
        ("https://localhost/v1/accept", "localhost"),
    ] {
        assert!(
            AuthenticatedWorkflowHttpProvider::new(endpoint, host, "webhook:billing", "test-token")
                .is_err(),
            "unsafe endpoint was accepted: {endpoint}"
        );
    }
}

#[test]
fn credentials_and_destination_are_validated() {
    assert!(AuthenticatedWorkflowHttpProvider::new(
        "https://events.example.com/accept",
        "events.example.com",
        "",
        "secret"
    )
    .is_err());
    assert!(AuthenticatedWorkflowHttpProvider::new(
        "https://events.example.com/accept",
        "events.example.com",
        "webhook:billing",
        ""
    )
    .is_err());
    assert!(AuthenticatedWorkflowHttpProvider::new(
        "https://events.example.com/accept",
        "events.example.com",
        "webhook:billing",
        "bad\r\nInjected: y"
    )
    .is_err());
    assert!(AuthenticatedWorkflowHttpProvider::new(
        "https://events.example.com/accept",
        "events.example.com",
        "webhook:billing",
        "not a bearer token"
    )
    .is_err());
}
