//! HTTP request capture for compiler-owned Web request bindings.
//!
//! The binding decoder is transport-neutral and borrows its inputs. This module
//! owns the HTTP-specific parsing needed to build those borrowed views without
//! coupling the decoder to the runtime server implementation.

use crate::web::request_bindings::{
    parse_cookie_header, parse_urlencoded, split_request_target, RequestBindingValues,
};
use std::collections::HashMap;

/// Owned HTTP request data derived once per matched route.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct HttpRequestBindingInputs {
    pub query: HashMap<String, String>,
    pub cookies: HashMap<String, String>,
    pub body: Option<String>,
    pub form: HashMap<String, String>,
}

impl HttpRequestBindingInputs {
    /// Parse transport-specific request sources once, before handler decoding.
    ///
    /// Query and cookie extraction are always available. Form values are only
    /// decoded for `application/x-www-form-urlencoded`; arbitrary request bodies
    /// must not be reinterpreted as form data merely because a handler declares
    /// a form binding.
    pub fn capture(target: &str, headers: &[(String, String)], body: &[u8]) -> Self {
        let (_, query) = split_request_target(target);

        let mut cookies = HashMap::new();
        for (_, value) in headers
            .iter()
            .filter(|(name, _)| name.eq_ignore_ascii_case("cookie"))
        {
            cookies.extend(parse_cookie_header(value));
        }

        let body_text = (!body.is_empty()).then(|| String::from_utf8_lossy(body).into_owned());
        let form = if is_urlencoded_form(headers) {
            parse_urlencoded(body)
        } else {
            HashMap::new()
        };

        Self {
            query,
            cookies,
            body: body_text,
            form,
        }
    }

    /// Borrow all request sources in the shape consumed by the transport-neutral
    /// binding decoder.
    pub fn values<'a>(
        &'a self,
        path: &'a HashMap<String, String>,
        headers: &'a [(String, String)],
    ) -> RequestBindingValues<'a> {
        RequestBindingValues {
            path,
            query: &self.query,
            headers,
            cookies: &self.cookies,
            body: self.body.as_deref(),
            form: &self.form,
        }
    }
}

fn is_urlencoded_form(headers: &[(String, String)]) -> bool {
    headers.iter().any(|(name, value)| {
        name.eq_ignore_ascii_case("content-type")
            && value.split(';').next().is_some_and(|media_type| {
                media_type
                    .trim()
                    .eq_ignore_ascii_case("application/x-www-form-urlencoded")
            })
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn captures_query_cookies_body_and_urlencoded_form() {
        let headers = vec![
            (
                "Content-Type".to_string(),
                "application/x-www-form-urlencoded; charset=utf-8".to_string(),
            ),
            (
                "Cookie".to_string(),
                "session=abc; theme=dark%20mode".to_string(),
            ),
        ];
        let captured = HttpRequestBindingInputs::capture(
            "/users?limit=25&term=hello+world",
            &headers,
            b"title=hello+world",
        );

        assert_eq!(captured.query.get("limit").map(String::as_str), Some("25"));
        assert_eq!(
            captured.query.get("term").map(String::as_str),
            Some("hello world")
        );
        assert_eq!(
            captured.cookies.get("theme").map(String::as_str),
            Some("dark mode")
        );
        assert_eq!(captured.body.as_deref(), Some("title=hello+world"));
        assert_eq!(
            captured.form.get("title").map(String::as_str),
            Some("hello world")
        );
    }

    #[test]
    fn arbitrary_body_is_not_reinterpreted_as_form_data() {
        let headers = vec![("Content-Type".to_string(), "application/json".to_string())];
        let captured = HttpRequestBindingInputs::capture(
            "/users?active=true",
            &headers,
            br#"{\"title\":\"hello\"}"#,
        );

        assert_eq!(
            captured.query.get("active").map(String::as_str),
            Some("true")
        );
        assert!(captured.form.is_empty());
        assert!(captured.body.is_some());
    }

    #[test]
    fn empty_body_is_missing_for_required_body_binding() {
        let captured = HttpRequestBindingInputs::capture("/", &[], b"");
        assert!(captured.body.is_none());
        assert!(captured.form.is_empty());
    }
}
