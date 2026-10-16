//! Experimental, bounded text HTTP transport for the Http.request effect.
//! This is not an actor transport and intentionally cannot follow redirects.
//! Keep legacy Http.get/post semantics unchanged while bringing explicit
//! methods, user-provided headers and response status to the new effect.
use std::collections::{BTreeMap, HashSet};
use std::io::Read;
use std::time::Duration;

const MAX_BODY_BYTES: usize = 4 * 1024 * 1024;
const MAX_HEADER_BYTES: usize = 64 * 1024;
const HTTP_TIMEOUT: Duration = Duration::from_secs(15);

/// Execute a single outbound request and return a UTF-8 JSON response:
/// {"status": integer, "ok": boolean, "headers": object, "body": string}.
///
/// The headers argument must be a JSON array of [name, value] string pairs.
/// Returns Err on invalid request, transport failure, over-limit payload,
/// or non-UTF-8 response. Non-2xx HTTP responses are valid responses with
/// status and body preserved. This function never retries or follows redirects.
pub fn request_text(
    method: &str,
    url: &str,
    headers_json: &str,
    body: &str,
) -> Result<String, String> {
    if !matches!(
        method,
        "GET" | "HEAD" | "POST" | "PUT" | "PATCH" | "DELETE" | "OPTIONS"
    ) {
        return Err("Http.request: unsupported HTTP method".to_string());
    }
    let authority = url
        .strip_prefix("https://")
        .or_else(|| url.strip_prefix("http://"))
        .and_then(|suffix| suffix.split(['/', '?', '#']).next())
        .ok_or_else(|| "Http.request: URL must be HTTP(S)".to_string())?;
    if authority.is_empty() || authority.contains('@') || authority.chars().any(char::is_whitespace)
    {
        return Err("Http.request: invalid URL authority".to_string());
    }
    if body.len() > MAX_BODY_BYTES {
        return Err("Http.request: request body exceeds 4 MiB limit".to_string());
    }
    if headers_json.len() > MAX_HEADER_BYTES {
        return Err("Http.request: request headers exceed 64 KiB limit".to_string());
    }
    let pairs: Vec<(String, String)> = serde_json::from_str(headers_json).map_err(|_| {
        "Http.request: headers must be a JSON array of [name,value] pairs".to_string()
    })?;
    if pairs.len() > 100 {
        return Err("Http.request: too many headers".to_string());
    }
    let mut seen = HashSet::new();
    let mut total_header_bytes = 0usize;
    for (name, value) in &pairs {
        total_header_bytes += name.len() + value.len();
        if total_header_bytes > MAX_HEADER_BYTES || name.len() > 256 || value.len() > 8192 {
            return Err("Http.request: headers exceed configured limits".to_string());
        }
        if name.is_empty()
            || !name
                .as_bytes()
                .iter()
                .all(|b| b.is_ascii_alphanumeric() || b"!#$%&'*+-.^_|~".contains(b) || *b == 0x60)
        {
            return Err("Http.request: invalid header name".to_string());
        }
        if !value
            .bytes()
            .all(|b| b == b'\t' || (0x20..=0x7e).contains(&b))
        {
            return Err("Http.request: invalid header value".to_string());
        }
        let lower = name.to_ascii_lowercase();
        if !seen.insert(lower.clone()) {
            return Err("Http.request: duplicate header name".to_string());
        }
        if matches!(
            lower.as_str(),
            "host" | "content-length" | "transfer-encoding" | "connection" | "proxy-authorization"
        ) {
            return Err("Http.request: transport-managed header cannot be overridden".to_string());
        }
    }

    let agent = ureq::AgentBuilder::new()
        .redirects(0)
        .timeout(HTTP_TIMEOUT)
        .build();
    let mut request = agent.request(method, url);
    for (name, value) in &pairs {
        request = request.set(name, value);
    }
    let response =
        match if body.is_empty() && matches!(method, "GET" | "HEAD" | "DELETE" | "OPTIONS") {
            request.call()
        } else {
            request.send_string(body)
        } {
            Ok(response) | Err(ureq::Error::Status(_, response)) => response,
            Err(error) => return Err(format!("Http.request: transport error: {error}")),
        };

    let status = response.status();
    let mut headers = BTreeMap::new();
    for name in response.headers_names() {
        if let Some(value) = response.header(&name) {
            headers.insert(name.to_ascii_lowercase(), value.to_string());
        }
    }
    let mut bytes = Vec::new();
    response
        .into_reader()
        .take((MAX_BODY_BYTES + 1) as u64)
        .read_to_end(&mut bytes)
        .map_err(|e| format!("Http.request: failed to read response body: {e}"))?;
    if bytes.len() > MAX_BODY_BYTES {
        return Err("Http.request: response body exceeds 4 MiB limit".to_string());
    }
    let body = String::from_utf8(bytes)
        .map_err(|_| "Http.request: response is not UTF-8 text".to_string())?;
    serde_json::to_string(&serde_json::json!({
        "status": status,
        "ok": (200..300).contains(&status),
        "headers": headers,
        "body": body,
    }))
    .map_err(|e| format!("Http.request: failed to serialize response: {e}"))
}

#[cfg(test)]
mod response_header_limit_tests {
    use super::*;
    
    #[test]
    fn rejects_aggregate_response_headers_above_limit() {
        let mut headers = BTreeMap::new();
        let mut used = 0usize;
        for i in 0..8 {
            insert_bounded_response_header(
                &mut headers,
                &mut used,
                &format!("x-long-{i}"),
                &"a".repeat(8192 - 8),
            )
            .expect("within response-header budget");
        }
        let error = insert_bounded_response_header(
            &mut headers,
            &mut used,
            "x-over",
            "one more header",
        )
        .expect_err("aggregate response-header bytes must be bounded");
        assert!(error.contains("response headers exceed"));
    }

    #[test]
    fn rejects_a_single_oversized_response_header() {
        let mut headers = BTreeMap::new();
        let mut used = 0usize;
        let error = insert_bounded_response_header(
            &mut headers,
            &mut used,
            "x-large",
            &"a".repeat(MAX_HEADER_BYTES + 1),
        )
        .expect_err("over-limit response header must be rejected");
        assert!(error.contains("response headers exceed"));
        assert_eq!(used, 0);
        assert!(headers.is_empty());
    }

    #[test]
    fn preserves_small_response_headers_and_normalizes_names() {
        let mut headers = BTreeMap::new();
        let mut used = 0usize;
        insert_bounded_response_header(&mut headers, &mut used, "X-Request-Id", "req-123")
            .expect("normal response header");
        assert_eq!(headers.get("x-request-id").map(String::as_str), Some("req-123"));
        assert_eq!(used, "X-Request-Id".len() + "req-123".len());
    }
}
