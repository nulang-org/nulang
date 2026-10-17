//! HTTP adaptation for transport-neutral Web request decode failures.
//!
//! Request decoding itself stays independent of HTTP. This module is the thin
//! boundary that serializes a [`RequestDecodeError`] as a problem response when
//! an HTTP transport chooses to surface the failure to a client.

use crate::web::request_bindings::RequestDecodeError;

pub const PROBLEM_JSON_CONTENT_TYPE: &str = "application/problem+json";

/// Transport-facing problem response produced without depending on the runtime
/// HTTP server implementation. Server adapters translate this DTO into their
/// native response type at the outer boundary.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ProblemHttpResponse {
    pub status: u16,
    pub headers: Vec<(String, String)>,
    pub body: Vec<u8>,
}

/// Convert a typed request decode failure into an HTTP problem response.
///
/// The error owns the client-vs-server classification: malformed/missing request
/// input becomes 400, while impossible binding-plan states remain 500. Keeping
/// that policy in the compiler/runtime error contract prevents HTTP adapters
/// from independently reclassifying the same failure.
pub fn request_decode_problem_response(error: &RequestDecodeError) -> ProblemHttpResponse {
    // Keep detailed compiler/runtime diagnostics available on the error object,
    // but never serialize internal handler names, slots or source metadata to
    // an HTTP client when the binding plan itself is invalid.
    let public_problem = if error.is_client_error() {
        error.problem_details()
    } else {
        serde_json::json!({
            "type": "about:blank",
            "title": "Internal Server Error",
            "status": 500,
            "code": "internal_server_error",
        })
    };
    let body = serde_json::to_vec(&public_problem).unwrap_or_else(|_| {
        br#"{"type":"about:blank","title":"Internal Server Error","status":500,"code":"internal_server_error"}"#
            .to_vec()
    });

    ProblemHttpResponse {
        status: error.http_status(),
        headers: vec![(
            "Content-Type".to_string(),
            PROBLEM_JSON_CONTENT_TYPE.to_string(),
        )],
        body,
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::web::bindings::{RouteBindingContract, RouteBindingSource};
    use crate::web::request_bindings::{
        bind_request_arguments, RequestBindingValues, RequestDecodeErrorKind,
    };
    use std::collections::HashMap;

    #[test]
    fn malformed_query_becomes_client_problem_response() {
        let empty = HashMap::new();
        let query = HashMap::from([("limit".to_string(), "many".to_string())]);
        let headers = Vec::new();
        let values = RequestBindingValues {
            path: &empty,
            query: &query,
            headers: &headers,
            cookies: &empty,
            body: None,
            form: &empty,
        };
        let binding = RouteBindingContract {
            source: RouteBindingSource::Query,
            source_name: "limit".to_string(),
            handler_param: "limit".to_string(),
            handler_index: 0,
            ty: Some("Int".to_string()),
        };
        let error = bind_request_arguments(&[binding], 1, &values).unwrap_err();
        assert_eq!(error.kind, RequestDecodeErrorKind::InvalidType);

        let response = request_decode_problem_response(&error);
        assert_eq!(response.status, 400);
        assert_eq!(
            response.headers,
            vec![(
                "Content-Type".to_string(),
                "application/problem+json".to_string()
            )]
        );
        let problem: serde_json::Value = serde_json::from_slice(&response.body).unwrap();
        assert_eq!(problem["status"], 400);
        assert_eq!(problem["code"], "invalid_request_input");
        assert_eq!(problem["source"], "query");
    }


    #[test]
    fn server_http_problem_never_exposes_internal_binding_diagnostics() {
        let error = RequestDecodeError {
            kind: RequestDecodeErrorKind::InvalidHandlerSlot,
            source: Some(RouteBindingSource::Header),
            source_name: Some("X-Internal-Tenant-Secret".to_string()),
            handler_param: Some("secret_tenant_validator".to_string()),
            handler_index: Some(987),
            expected_type: Some("InternalCapability".to_string()),
            message: "secret_tenant_validator points to impossible slot 987".to_string(),
        };

        // The transport-neutral error remains useful to server-side diagnostics.
        assert!(error.problem_details()["detail"]
            .as_str()
            .unwrap()
            .contains("secret_tenant_validator"));

        // The externally serialized problem must not disclose internal details.
        let response = request_decode_problem_response(&error);
        assert_eq!(response.status, 500);
        let problem: serde_json::Value = serde_json::from_slice(&response.body).unwrap();
        assert_eq!(problem["status"], 500);
        assert_eq!(problem["code"], "internal_server_error");
        let exposed = String::from_utf8(response.body).unwrap();
        for forbidden in [
            "secret_tenant_validator",
            "X-Internal-Tenant-Secret",
            "InternalCapability",
            "987",
            "invalid_handler_slot",
            "handler_param",
            "handler_index",
            "expected_type",
            "source_name",
        ] {
            assert!(!exposed.contains(forbidden), "exposed {forbidden}: {exposed}");
        }
    }

    #[test]
    fn binding_plan_failure_stays_server_problem_response() {
        let error = RequestDecodeError {
            kind: RequestDecodeErrorKind::IncompleteBindingPlan,
            source: None,
            source_name: None,
            handler_param: None,
            handler_index: Some(1),
            expected_type: None,
            message: "handler parameter slot 1 has no request binding".to_string(),
        };

        let response = request_decode_problem_response(&error);
        assert_eq!(response.status, 500);
        let problem: serde_json::Value = serde_json::from_slice(&response.body).unwrap();
        assert_eq!(problem["code"], "internal_server_error");
        assert_eq!(problem["title"], "Internal Server Error");
        assert_eq!(problem["type"], "about:blank");
        assert!(problem.get("handler_index").is_none());
        assert!(problem.get("detail").is_none());
    }
}
