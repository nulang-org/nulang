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
    let body = serde_json::to_vec(&error.problem_details()).unwrap_or_else(|_| {
        br#"{\"type\":\"urn:nulang:web:problem-serialization\",\"title\":\"Internal Server Error\",\"status\":500}"#
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
        assert_eq!(problem["code"], "incomplete_binding_plan");
        assert_eq!(problem["handler_index"], 1);
    }
}
