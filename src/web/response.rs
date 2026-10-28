//! Explicit web response formats derived from handler result types.
//!
//! Unknown and legacy result types retain the existing HTML behavior. Only
//! nominal `Json` and `Text` results opt into non-HTML response handling.

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ResponseKind {
    Html,
    Json,
    Text,
}

impl ResponseKind {
    /// A type is an explicit media contract only for one of these nominal
    /// result types; arbitrary domain return types do not imply JSON.
    pub fn explicit(ty: Option<&str>) -> Option<Self> {
        match ty {
            Some("Html") => Some(Self::Html),
            Some("Json") => Some(Self::Json),
            Some("Text") => Some(Self::Text),
            _ => None,
        }
    }

    pub fn from_return_type(ty: Option<&str>) -> Self {
        Self::explicit(ty).unwrap_or(Self::Html)
    }

    pub const fn content_type(self) -> &'static str {
        match self {
            Self::Html => "text/html; charset=utf-8",
            Self::Json => "application/json; charset=utf-8",
            Self::Text => "text/plain; charset=utf-8",
        }
    }

    pub const fn openapi_media_type(self) -> &'static str {
        match self {
            Self::Html => "text/html",
            Self::Json => "application/json",
            Self::Text => "text/plain",
        }
    }

    /// Validate JSON before sending it with a JSON content type. Only HTML
    /// responses receive the browser's progressive-enhancement script.
    pub fn encode(self, rendered: &str) -> Result<Vec<u8>, &'static str> {
        match self {
            Self::Html => {
                Ok(crate::web::reactivity::inject_client_runtime_script(rendered).into_bytes())
            }
            Self::Text => Ok(rendered.as_bytes().to_vec()),
            Self::Json => {
                serde_json::from_str::<serde_json::Value>(rendered)
                    .map_err(|_| "JSON route returned an invalid JSON document")?;
                Ok(rendered.as_bytes().to_vec())
            }
        }
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
    fn plain_text_does_not_inject_scripts_or_escape_content() {
        let bytes = ResponseKind::Text.encode("<div>Hi</div>").unwrap();
        assert_eq!(bytes, b"<div>Hi</div>");
    }
}
