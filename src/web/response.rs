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
    NoContent,
    Problem,
    InvalidForm,
}

/// Transport-neutral result of executing a compiler-validated route.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct RenderedResponse {
    pub status: u16,
    pub content_type: Option<&'static str>,
    pub body: Vec<u8>,
}

impl ResponseKind {
    pub fn explicit(ty: Option<&str>) -> Option<Self> {
        match ty {
            Some("Html") => Some(Self::Html),
            Some("Json") => Some(Self::Json),
            Some("Text") => Some(Self::Text),
            Some("Created") => Some(Self::Created),
            Some("NoContent") => Some(Self::NoContent),
            Some("Problem") => Some(Self::Problem),
            Some("InvalidForm") => Some(Self::InvalidForm),
            _ => None,
        }
    }

    pub fn from_return_type(ty: Option<&str>) -> Self {
        Self::explicit(ty).unwrap_or(Self::Html)
    }

    pub const fn content_type(self) -> &'static str {
        match self {
            Self::Html | Self::InvalidForm => "text/html; charset=utf-8",
            Self::Json | Self::Created => "application/json; charset=utf-8",
            Self::Text => "text/plain; charset=utf-8",
            Self::Problem => "application/problem+json",
            Self::NoContent => "",
        }
    }

    pub const fn openapi_media_type(self) -> &'static str {
        match self {
            Self::Html | Self::InvalidForm => "text/html",
            Self::Json | Self::Created => "application/json",
            Self::Text => "text/plain",
            Self::Problem => "application/problem+json",
            Self::NoContent => "",
        }
    }

    /// HTTP response semantics are determined from the statically resolved
    /// nominal handler return type. Problem is the sole dynamic status result:
    /// its RFC-style JSON status must be an integer error code (400..=599).
    pub fn render(self, rendered: &str) -> Result<RenderedResponse, &'static str> {
        let (status, body) = match self {
            Self::Html => (
                200,
                crate::web::reactivity::inject_client_runtime_script(rendered).into_bytes(),
            ),
            Self::Text => (200, rendered.as_bytes().to_vec()),
            Self::InvalidForm => (422, rendered.as_bytes().to_vec()),
            Self::Json | Self::Created => {
                serde_json::from_str::<serde_json::Value>(rendered)
                    .map_err(|_| "JSON route returned an invalid JSON document")?;
                (
                    if self == Self::Created { 201 } else { 200 },
                    rendered.as_bytes().to_vec(),
                )
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
        })
    }

    /// Retain the earlier body-only API for existing callers and tests.
    pub fn encode(self, rendered: &str) -> Result<Vec<u8>, &'static str> {
        self.render(rendered).map(|response| response.body)
    }
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
    fn invalid_form_result_is_exact_html_with_http_422_and_no_script_injection() {
        assert_eq!(
            ResponseKind::from_return_type(Some("InvalidForm")),
            ResponseKind::InvalidForm
        );
        let fragment = "<form id=\"edit\"><span>Required</span></form>";
        let reply = ResponseKind::InvalidForm
            .render(fragment)
            .expect("render form error");
        assert_eq!(reply.status, 422);
        assert_eq!(reply.content_type, Some("text/html; charset=utf-8"));
        assert_eq!(reply.body, fragment.as_bytes());
    }

    #[test]
    fn plain_text_does_not_inject_scripts_or_escape_content() {
        let bytes = ResponseKind::Text.encode("<div>Hi</div>").unwrap();
        assert_eq!(bytes, b"<div>Hi</div>");
    }
}
