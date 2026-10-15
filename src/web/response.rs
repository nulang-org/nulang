//! Compiler-owned Web response semantics.
//!
//! Handler return types are the source of truth. This module projects the
//! subset with explicit transport semantics into a reusable contract consumed
//! by runtime dispatch, deployment IR, OpenAPI, adapters, and Nulang Cloud.
//! Unknown response types intentionally return `None` so legacy HTML behavior
//! remains backwards compatible rather than guessing protocol semantics.

use serde::{Deserialize, Serialize};

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ResponseBodyKind {
    Html,
    Json,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ResponseContract {
    pub kind: ResponseBodyKind,
    /// Base media type. HTTP-specific parameters such as charset stay separate.
    pub media_type: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub charset: Option<String>,
    /// Logical payload type for typed bodies such as Json[T].
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub payload_type: Option<String>,
    /// Whether the Nulang browser runtime may be injected into the response.
    pub inject_client_runtime: bool,
}

impl ResponseContract {
    fn html() -> Self {
        Self {
            kind: ResponseBodyKind::Html,
            media_type: "text/html".to_string(),
            charset: Some("utf-8".to_string()),
            payload_type: None,
            inject_client_runtime: true,
        }
    }

    fn json(payload_type: String) -> Self {
        Self {
            kind: ResponseBodyKind::Json,
            media_type: "application/json".to_string(),
            charset: None,
            payload_type: Some(payload_type),
            inject_client_runtime: false,
        }
    }

    pub fn http_content_type(&self) -> String {
        match &self.charset {
            Some(charset) => format!("{}; charset={charset}", self.media_type),
            None => self.media_type.clone(),
        }
    }
}

/// Project a declared source-level response type into transport semantics.
///
/// Only types with unambiguous Web meaning are recognized. `String`, arbitrary
/// domain types, and missing annotations retain the historical HTML-oriented
/// behavior until the application opts into an explicit response wrapper.
pub fn response_contract(response_type: Option<&str>) -> Option<ResponseContract> {
    let ty = response_type?.trim();
    match ty {
        "Html" | "RawHtml" => Some(ResponseContract::html()),
        _ => json_payload_type(ty).map(ResponseContract::json),
    }
}

fn json_payload_type(ty: &str) -> Option<String> {
    let payload = ty.strip_prefix("Json[")?.strip_suffix(']')?.trim();
    if payload.is_empty() {
        return None;
    }
    Some(payload.to_string())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn recognizes_html_without_reclassifying_string() {
        let html = response_contract(Some("Html")).unwrap();
        assert_eq!(html.kind, ResponseBodyKind::Html);
        assert_eq!(html.http_content_type(), "text/html; charset=utf-8");
        assert!(html.inject_client_runtime);
        assert!(response_contract(Some("String")).is_none());
        assert!(response_contract(None).is_none());
    }

    #[test]
    fn recognizes_json_and_preserves_nested_payload_type() {
        let json = response_contract(Some("Json[Result[User, ApiError]]")).unwrap();
        assert_eq!(json.kind, ResponseBodyKind::Json);
        assert_eq!(json.http_content_type(), "application/json");
        assert_eq!(
            json.payload_type.as_deref(),
            Some("Result[User, ApiError]")
        );
        assert!(!json.inject_client_runtime);
    }

    #[test]
    fn rejects_empty_json_payload_marker() {
        assert!(response_contract(Some("Json[]")).is_none());
    }
}
