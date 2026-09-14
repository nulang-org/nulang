//! Compiler-owned Web response semantics.
//!
//! A handler's declared Nulang return type remains the source of truth. This
//! module projects the small subset whose transport semantics are explicit into
//! a reusable contract consumed by runtime dispatch, deployment IR, OpenAPI,
//! tests, and future adapters. Unknown response types intentionally return
//! `None` so legacy HTML-oriented behavior remains backwards compatible.

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
    pub media_type: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub payload_type: Option<String>,
    /// Whether the HTML browser runtime should be injected into the rendered
    /// body. Non-HTML responses must never receive browser hydration scripts.
    pub inject_client_runtime: bool,
}

impl ResponseContract {
    fn html() -> Self {
        Self {
            kind: ResponseBodyKind::Html,
            media_type: "text/html; charset=utf-8".to_string(),
            payload_type: None,
            inject_client_runtime: true,
        }
    }

    fn json(payload_type: String) -> Self {
        Self {
            kind: ResponseBodyKind::Json,
            media_type: "application/json".to_string(),
            payload_type: Some(payload_type),
            inject_client_runtime: false,
        }
    }
}

/// Project a source-level handler return type into explicit transport semantics.
///
/// This first slice deliberately recognizes only response types whose media
/// contract is unambiguous. `String`, domain types, and missing annotations keep
/// the historical HTML-oriented runtime behavior rather than being reclassified.
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
        assert_eq!(html.media_type, "text/html; charset=utf-8");
        assert!(html.inject_client_runtime);
        assert!(response_contract(Some("String")).is_none());
        assert!(response_contract(None).is_none());
    }

    #[test]
    fn recognizes_json_and_preserves_nested_payload_type() {
        let json = response_contract(Some("Json[Result[User, ApiError]]")).unwrap();
        assert_eq!(json.kind, ResponseBodyKind::Json);
        assert_eq!(json.media_type, "application/json");
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
