//! HTTP request capture for compiler-owned Web request bindings.
//!
//! The binding decoder is transport-neutral and borrows its inputs. This module
//! owns the HTTP-specific parsing needed to build those borrowed views without
//! coupling the decoder to the runtime server implementation.

use crate::web::request_bindings::{
    parse_cookie_header, parse_urlencoded, split_request_target, RequestBindingValues,
};
use nulang_ui_protocol::{decode_host_message, HostToRuntimeMessage};
use std::collections::HashMap;

/// Owned HTTP request data derived once per matched route.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct HttpRequestBindingInputs {
    pub query: HashMap<String, String>,
    pub cookies: HashMap<String, String>,
    pub body: Option<String>,
    pub form: HashMap<String, String>,
    /// Validated renderer-neutral UI/action message carried alongside a
    /// compatibility form submission. Invalid or unsupported envelopes are
    /// ignored rather than becoming runtime authority.
    pub ui_message: Option<HostToRuntimeMessage>,
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
        let ui_message = form
            .get("__nulang_ui_message")
            .and_then(|encoded| decode_host_message(encoded).ok())
            .filter(|message| message.validate().is_ok());

        Self {
            query,
            cookies,
            body: body_text,
            form,
            ui_message,
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
    #[test]
    fn extracts_and_validates_ui_action_message_from_form() {
        let message = nulang_ui_protocol::HostToRuntimeMessage::invoke_action(
            nulang_ui_protocol::ActionRequest {
                document_id: "app".into(),
                revision: nulang_ui_protocol::Revision(7),
                action_id: "save".into(),
                placement: nulang_ui_protocol::ActionPlacement::Server,
                correlation_id: "corr-1".into(),
                idempotency_key: "idem-1".into(),
                payload: nulang_ui_protocol::WireValue::Null,
            },
        );
        let encoded = nulang_ui_protocol::encode_host_message(&message).unwrap();
        let body = format!(
            "__nulang_action=save&__nulang_ui_message={}",
            percent_encode_form_value(&encoded)
        );
        let headers = vec![(
            "Content-Type".to_string(),
            "application/x-www-form-urlencoded".to_string(),
        )];
        let captured = HttpRequestBindingInputs::capture("/", &headers, body.as_bytes());

        assert_eq!(captured.ui_message, Some(message));
    }

    #[test]
    fn invalid_ui_action_message_is_not_accepted() {
        let headers = vec![(
            "Content-Type".to_string(),
            "application/x-www-form-urlencoded".to_string(),
        )];
        let captured = HttpRequestBindingInputs::capture(
            "/",
            &headers,
            b"__nulang_ui_message=%7B%22type%22%3A%22invoke_action%22%7D",
        );
        assert!(captured.ui_message.is_none());
    }

    fn percent_encode_form_value(value: &str) -> String {
        value
            .bytes()
            .map(|byte| match byte {
                b'A'..=b'Z' | b'a'..=b'z' | b'0'..=b'9' | b'-' | b'_' | b'.' | b'~' => {
                    (byte as char).to_string()
                }
                _ => format!("%{byte:02X}"),
            })
            .collect()
    }

}
