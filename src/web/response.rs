//! Explicit web response formats derived from handler result types.
//!
//! Unknown and legacy result types retain the existing HTML behavior. Only
//! nominal `Json` and `Text` results opt into non-HTML response handling.

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ResponseKind {
    Html,
    Json,
    Text,
    Created,
    CreatedAt,
    NoContent,
    Problem,
}

/// Transport-neutral result of executing a compiler-validated route.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct RenderedResponse {
    pub status: u16,
    pub content_type: Option<&'static str>,
    pub body: Vec<u8>,
    /// Trusted validated same-origin Location header for CreatedAt only.
    pub location: Option<String>,
}

impl ResponseKind {
    pub fn explicit(ty: Option<&str>) -> Option<Self> {
        match ty {
            Some("Html") => Some(Self::Html),
            Some("Json") => Some(Self::Json),
            Some("Text") => Some(Self::Text),
            Some("Created") => Some(Self::Created),
            Some("CreatedAt") => Some(Self::CreatedAt),
            Some("NoContent") => Some(Self::NoContent),
            Some("Problem") => Some(Self::Problem),
            _ => None,
        }
    }

    pub fn from_return_type(ty: Option<&str>) -> Self {
        Self::explicit(ty).unwrap_or(Self::Html)
    }

    pub const fn content_type(self) -> &'static str {
        match self {
            Self::Html => "text/html; charset=utf-8",
            Self::Json | Self::Created | Self::CreatedAt => "application/json; charset=utf-8",
            Self::Text => "text/plain; charset=utf-8",
            Self::Problem => "application/problem+json",
            Self::NoContent => "",
        }
    }

    pub const fn openapi_media_type(self) -> &'static str {
        match self {
            Self::Html => "text/html",
            Self::Json | Self::Created | Self::CreatedAt => "application/json",
            Self::Text => "text/plain",
            Self::Problem => "application/problem+json",
            Self::NoContent => "",
        }
    }

    /// HTTP response semantics are determined from the statically resolved
    /// nominal handler return type. Problem is the sole dynamic status result:
    /// its RFC-style JSON status must be an integer error code (400..=599).
    pub fn render(self, rendered: &str) -> Result<RenderedResponse, &'static str> {
        let mut location = None;
        let (status, body) = match self {
            Self::Html => (
                200,
                crate::web::reactivity::inject_client_runtime_script(rendered).into_bytes(),
            ),
            Self::Text => (200, rendered.as_bytes().to_vec()),
            Self::Json | Self::Created => {
                serde_json::from_str::<serde_json::Value>(rendered)
                    .map_err(|_| "JSON route returned an invalid JSON document")?;
                (
                    if self == Self::Created { 201 } else { 200 },
                    rendered.as_bytes().to_vec(),
                )
            }
            Self::CreatedAt => {
                let envelope: serde_json::Value = serde_json::from_str(rendered)
                    .map_err(|_| "CreatedAt result envelope is invalid JSON")?;
                let fields = envelope
                    .as_object()
                    .ok_or("CreatedAt result must be an object")?;
                if fields.len() != 2 {
                    return Err("CreatedAt envelope requires only location and body_json");
                }
                let raw_location = fields
                    .get("location")
                    .and_then(serde_json::Value::as_str)
                    .ok_or("CreatedAt location must be a string")?;
                if !valid_created_location(raw_location) {
                    return Err("CreatedAt location must be a safe same-origin resource path");
                }
                let json_body = fields
                    .get("body_json")
                    .and_then(serde_json::Value::as_str)
                    .ok_or("CreatedAt body_json must be a string")?;
                serde_json::from_str::<serde_json::Value>(json_body)
                    .map_err(|_| "CreatedAt body must be complete valid JSON")?;
                location = Some(raw_location.to_string());
                (201, json_body.as_bytes().to_vec())
            }
            Self::NoContent => {
                if !rendered.is_empty() {
                    return Err("NoContent handler returned a nonempty body");
                }
                (204, Vec::new())
            }
            Self::Problem => {
                let document: serde_json::Value = serde_json::from_str(rendered)
                    .map_err(|_| "Problem handler returned invalid JSON")?;
                let object = document
                    .as_object()
                    .ok_or("Problem must be a JSON object")?;
                let status = object
                    .get("status")
                    .and_then(serde_json::Value::as_u64)
                    .ok_or("Problem status must be an integer")?;
                if !(400..=599).contains(&status) {
                    return Err("Problem status must be in the HTTP error range");
                }
                if !object
                    .get("title")
                    .is_some_and(serde_json::Value::is_string)
                {
                    return Err("Problem title must be a string");
                }
                if let Some(detail) = object.get("detail") {
                    if !detail.is_string() {
                        return Err("Problem detail must be a string");
                    }
                }
                (status as u16, rendered.as_bytes().to_vec())
            }
        };
        Ok(RenderedResponse {
            status,
            content_type: (self != Self::NoContent).then(|| self.content_type()),
            body,
            location,
        })
    }

    /// Retain the earlier body-only API for existing callers and tests.
    pub fn encode(self, rendered: &str) -> Result<Vec<u8>, &'static str> {
        self.render(rendered).map(|response| response.body)
    }
}

/// Deliberately strict origin-relative URL grammar. Never emit authority
/// components, fragments, invalid percent escapes or encoded path separators
/// into Location headers. This avoids external redirects and header splitting.
fn valid_created_location(value: &str) -> bool {
    if value.len() > 2048 || !value.starts_with('/') || value.starts_with("//") || value.is_empty()
    {
        return false;
    }
    let bytes = value.as_bytes();
    if bytes
        .iter()
        .any(|byte| !byte.is_ascii_graphic() || matches!(byte, b'\\' | b'#' | b'"' | b'<' | b'>'))
    {
        return false;
    }
    let path = value.split('?').next().unwrap_or(value);
    if path.split('/').any(|seg| seg == "." || seg == "..") {
        return false;
    }
    let mut i = 0;
    while i < bytes.len() {
        if bytes[i] == b'%' {
            if i + 2 >= bytes.len() {
                return false;
            }
            let Some(a) = (bytes[i + 1] as char).to_digit(16) else {
                return false;
            };
            let Some(b) = (bytes[i + 2] as char).to_digit(16) else {
                return false;
            };
            let decoded = (a * 16 + b) as u8;
            if decoded <= 0x20 || decoded >= 0x7f || matches!(decoded, b'/' | b'\\' | b'.' | b'#') {
                return false;
            }
            i += 3;
        } else {
            i += 1;
        }
    }
    true
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn legacy_types_remain_html_while_explicit_types_choose_content_type() {
        assert_eq!(
            ResponseKind::from_return_type(Some("User")),
            ResponseKind::Html
        );
        assert_eq!(ResponseKind::from_return_type(None), ResponseKind::Html);
        assert_eq!(ResponseKind::explicit(Some("User")), None);
        assert_eq!(
            ResponseKind::explicit(Some("Json")),
            Some(ResponseKind::Json)
        );
        assert_eq!(
            ResponseKind::Json.content_type(),
            "application/json; charset=utf-8"
        );
    }

    #[test]
    fn json_requires_a_complete_json_document() {
        assert_eq!(
            ResponseKind::Json.encode("{\"ok\":true}").unwrap(),
            b"{\"ok\":true}"
        );
        assert!(ResponseKind::Json.encode("{bad}").is_err());
        assert!(ResponseKind::Json.encode("true false").is_err());
    }

    #[test]
    fn created_with_location_emits_resource_header_and_json_body() {
        let result = ResponseKind::CreatedAt
            .render(r#"{"location":"/users/42","body_json":"{\"id\":42}"}"#)
            .unwrap();
        assert_eq!(result.status, 201);
        assert_eq!(result.location.as_deref(), Some("/users/42"));
        assert_eq!(result.body, br#"{"id":42}"#);
        assert_eq!(result.content_type, Some("application/json; charset=utf-8"));
    }

    #[test]
    fn created_with_location_rejects_invalid_or_off_origin_urls() {
        for location in [
            "https://evil.example/x",
            "//evil.example/x",
            "/\\evil.example",
            "/users/../admin",
            "/users/%2f%2fevil.example",
            "/users/42\r\nX-Injected: 1",
            "",
            "/users/42#fragment",
        ] {
            let envelope = serde_json::json!({"location": location, "body_json": "{}"}).to_string();
            assert!(
                ResponseKind::CreatedAt.render(&envelope).is_err(),
                "accepted {location:?}"
            );
        }
        for body in ["broken json", "true false"] {
            let envelope =
                serde_json::json!({"location": "/users/42", "body_json": body}).to_string();
            assert!(ResponseKind::CreatedAt.render(&envelope).is_err());
        }
    }

    #[test]
    fn created_and_no_content_have_correct_wire_semantics() {
        let created = ResponseKind::Created.render(r#"{"id":7}"#).unwrap();
        assert_eq!(created.status, 201);
        assert_eq!(
            created.content_type,
            Some("application/json; charset=utf-8")
        );
        assert_eq!(created.body, br#"{"id":7}"#);
        let empty = ResponseKind::NoContent.render("").unwrap();
        assert_eq!(empty.status, 204);
        assert!(empty.body.is_empty());
        assert_eq!(empty.content_type, None);
        assert!(ResponseKind::NoContent.render("unexpected").is_err());
        assert!(ResponseKind::Created.render("not json").is_err());
    }

    #[test]
    fn problem_status_is_validated_and_bound_to_document() {
        let problem = ResponseKind::Problem
            .render(r#"{"status":409,"title":"Conflict","detail":"already exists"}"#)
            .unwrap();
        assert_eq!(problem.status, 409);
        assert_eq!(problem.content_type, Some("application/problem+json"));
        for raw in [
            r#"{"status":200,"title":"ok"}"#,
            r#"{"status":409.3,"title":"bad"}"#,
            r#"{"status":409}"#,
            "not json",
        ] {
            assert!(ResponseKind::Problem.render(raw).is_err(), "accepted {raw}");
        }
    }

    #[test]
    fn plain_text_does_not_inject_scripts_or_escape_content() {
        let bytes = ResponseKind::Text.encode("<div>Hi</div>").unwrap();
        assert_eq!(bytes, b"<div>Hi</div>");
    }
}
