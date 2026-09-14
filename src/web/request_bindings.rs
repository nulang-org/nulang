//! Transport-neutral request value extraction for compiler-produced Web bindings.
//!
//! The Web Contract IR names six request sources (`path`, `query`, `header`,
//! `cookie`, `body`, and `form`). This module gives transports one shared,
//! deterministic decoder for those sources without coupling the compiler IR to
//! `HttpRequest` or ambient request state.

use crate::bytecode::Constant;
use crate::web::bindings::{RouteBindingContract, RouteBindingSource};
use crate::web::runtime_bindings::BoundRouteArgument;
use std::collections::HashMap;
use std::fmt;

#[derive(Debug, Default)]
pub struct RequestBindingValues<'a> {
    pub path: &'a HashMap<String, String>,
    pub query: &'a HashMap<String, String>,
    pub headers: &'a [(String, String)],
    pub cookies: &'a HashMap<String, String>,
    pub body: Option<&'a str>,
    pub form: &'a HashMap<String, String>,
}

impl<'a> RequestBindingValues<'a> {
    pub fn path_only(path: &'a HashMap<String, String>) -> Self {
        Self { path, ..Self::default() }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum RequestDecodeErrorKind {
    Missing,
    InvalidType,
    DuplicateHandlerSlot,
    InvalidHandlerSlot,
    IncompleteBindingPlan,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct RequestDecodeError {
    pub kind: RequestDecodeErrorKind,
    pub source: Option<RouteBindingSource>,
    pub source_name: Option<String>,
    pub handler_param: Option<String>,
    pub handler_index: Option<usize>,
    pub expected_type: Option<String>,
    pub message: String,
}

impl fmt::Display for RequestDecodeError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result { f.write_str(&self.message) }
}
impl std::error::Error for RequestDecodeError {}

pub fn bind_request_arguments(
    bindings: &[RouteBindingContract],
    handler_param_count: usize,
    values: &RequestBindingValues<'_>,
) -> Result<Vec<BoundRouteArgument>, RequestDecodeError> {
    if handler_param_count > u8::MAX as usize {
        return Err(RequestDecodeError {
            kind: RequestDecodeErrorKind::InvalidHandlerSlot,
            source: None,
            source_name: None,
            handler_param: None,
            handler_index: None,
            expected_type: None,
            message: format!("route handler has {handler_param_count} parameters; VM call ABI supports at most {}", u8::MAX),
        });
    }

    let mut slots: Vec<Option<BoundRouteArgument>> = vec![None; handler_param_count];
    for binding in bindings {
        if binding.handler_index >= handler_param_count {
            return Err(binding_error(binding, RequestDecodeErrorKind::InvalidHandlerSlot, format!("binding for '{}' targets handler slot {} but handler has {} parameters", binding.handler_param, binding.handler_index, handler_param_count)));
        }
        if slots[binding.handler_index].is_some() {
            return Err(binding_error(binding, RequestDecodeErrorKind::DuplicateHandlerSlot, format!("multiple request bindings target handler slot {}", binding.handler_index)));
        }

        let raw = raw_value(binding, values).ok_or_else(|| binding_error(
            binding,
            RequestDecodeErrorKind::Missing,
            format!("missing {} input '{}' for handler parameter '{}'", source_name(binding.source), binding.source_name, binding.handler_param),
        ))?;
        let value = decode_scalar_constant(raw, binding.ty.as_deref()).map_err(|message| binding_error(
            binding,
            RequestDecodeErrorKind::InvalidType,
            format!("{} input '{}' for handler parameter '{}': {message}", source_name(binding.source), binding.source_name, binding.handler_param),
        ))?;
        slots[binding.handler_index] = Some(BoundRouteArgument { handler_index: binding.handler_index, value });
    }

    slots.into_iter().enumerate().map(|(index, slot)| {
        slot.ok_or_else(|| RequestDecodeError {
            kind: RequestDecodeErrorKind::IncompleteBindingPlan,
            source: None,
            source_name: None,
            handler_param: None,
            handler_index: Some(index),
            expected_type: None,
            message: format!("handler parameter slot {index} has no request binding"),
        })
    }).collect()
}

fn binding_error(binding: &RouteBindingContract, kind: RequestDecodeErrorKind, message: String) -> RequestDecodeError {
    RequestDecodeError {
        kind,
        source: Some(binding.source),
        source_name: Some(binding.source_name.clone()),
        handler_param: Some(binding.handler_param.clone()),
        handler_index: Some(binding.handler_index),
        expected_type: binding.ty.clone(),
        message,
    }
}

fn raw_value<'a>(binding: &RouteBindingContract, values: &'a RequestBindingValues<'a>) -> Option<&'a str> {
    match binding.source {
        RouteBindingSource::Path => values.path.get(&binding.source_name).map(String::as_str),
        RouteBindingSource::Query => values.query.get(&binding.source_name).map(String::as_str),
        RouteBindingSource::Header => values.headers.iter().find(|(name, _)| name.eq_ignore_ascii_case(&binding.source_name)).map(|(_, value)| value.as_str()),
        RouteBindingSource::Cookie => values.cookies.get(&binding.source_name).map(String::as_str),
        RouteBindingSource::Body => values.body,
        RouteBindingSource::Form => values.form.get(&binding.source_name).map(String::as_str),
    }
}

fn source_name(source: RouteBindingSource) -> &'static str {
    match source {
        RouteBindingSource::Path => "path",
        RouteBindingSource::Query => "query",
        RouteBindingSource::Header => "header",
        RouteBindingSource::Cookie => "cookie",
        RouteBindingSource::Body => "body",
        RouteBindingSource::Form => "form",
    }
}

pub fn decode_scalar_constant(raw: &str, ty: Option<&str>) -> Result<Constant, String> {
    match ty.map(str::trim) {
        Some("Int") => raw.parse::<i64>().map(Constant::Int).map_err(|_| format!("expected Int, got '{raw}'")),
        Some("Float") => raw.parse::<f64>().map(Constant::Float).map_err(|_| format!("expected Float, got '{raw}'")),
        Some("Bool") => match raw {
            "true" => Ok(Constant::Bool(true)),
            "false" => Ok(Constant::Bool(false)),
            _ => Err(format!("expected Bool ('true' or 'false'), got '{raw}'")),
        },
        _ => Ok(Constant::String(raw.to_string())),
    }
}

pub fn split_request_target(target: &str) -> (String, HashMap<String, String>) {
    let (path, query) = target.split_once('?').unwrap_or((target, ""));
    (path.to_string(), parse_urlencoded(query.as_bytes()))
}

pub fn parse_urlencoded(input: &[u8]) -> HashMap<String, String> {
    let input = String::from_utf8_lossy(input);
    let mut out = HashMap::new();
    for part in input.split('&') {
        if part.is_empty() { continue; }
        let (key, value) = part.split_once('=').unwrap_or((part, ""));
        out.insert(percent_decode(key), percent_decode(value));
    }
    out
}

pub fn parse_cookie_header(header: &str) -> HashMap<String, String> {
    let mut out = HashMap::new();
    for part in header.split(';') {
        let part = part.trim();
        if let Some((key, value)) = part.split_once('=') {
            out.insert(key.trim().to_string(), percent_decode(value.trim()));
        }
    }
    out
}

fn percent_decode(input: &str) -> String {
    let mut out = String::with_capacity(input.len());
    let bytes = input.as_bytes();
    let mut index = 0;
    while index < bytes.len() {
        match bytes[index] {
            b'+' => { out.push(' '); index += 1; }
            b'%' if index + 2 < bytes.len() => {
                if let (Some(high), Some(low)) = (hex(bytes[index + 1]), hex(bytes[index + 2])) {
                    out.push(((high << 4) | low) as char);
                    index += 3;
                } else {
                    out.push('%'); index += 1;
                }
            }
            byte => { out.push(byte as char); index += 1; }
        }
    }
    out
}

fn hex(byte: u8) -> Option<u8> {
    match byte {
        b'0'..=b'9' => Some(byte - b'0'),
        b'a'..=b'f' => Some(byte - b'a' + 10),
        b'A'..=b'F' => Some(byte - b'A' + 10),
        _ => None,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn binding(source: RouteBindingSource, source_name: &str, handler_param: &str, index: usize, ty: Option<&str>) -> RouteBindingContract {
        RouteBindingContract { source, source_name: source_name.to_string(), handler_param: handler_param.to_string(), handler_index: index, ty: ty.map(str::to_string) }
    }

    #[test]
    fn binds_every_reserved_request_source() {
        let path = HashMap::from([("id".to_string(), "42".to_string())]);
        let query = HashMap::from([("limit".to_string(), "25".to_string())]);
        let headers = vec![("X-Trace-Id".to_string(), "abc".to_string())];
        let cookies = HashMap::from([("session".to_string(), "sess-1".to_string())]);
        let form = HashMap::from([("title".to_string(), "Hello".to_string())]);
        let values = RequestBindingValues { path: &path, query: &query, headers: &headers, cookies: &cookies, body: Some("raw-body"), form: &form };
        let bindings = vec![
            binding(RouteBindingSource::Path, "id", "id", 0, Some("Int")),
            binding(RouteBindingSource::Query, "limit", "limit", 1, Some("Int")),
            binding(RouteBindingSource::Header, "x-trace-id", "trace_id", 2, Some("String")),
            binding(RouteBindingSource::Cookie, "session", "session", 3, Some("String")),
            binding(RouteBindingSource::Body, "body", "body", 4, Some("String")),
            binding(RouteBindingSource::Form, "title", "title", 5, Some("String")),
        ];
        let args = bind_request_arguments(&bindings, 6, &values).unwrap();
        assert_eq!(args[0].value, Constant::Int(42));
        assert_eq!(args[1].value, Constant::Int(25));
        assert_eq!(args[2].value, Constant::String("abc".to_string()));
        assert_eq!(args[3].value, Constant::String("sess-1".to_string()));
        assert_eq!(args[4].value, Constant::String("raw-body".to_string()));
        assert_eq!(args[5].value, Constant::String("Hello".to_string()));
    }

    #[test]
    fn decode_errors_are_structured() {
        let query = HashMap::from([("limit".to_string(), "many".to_string())]);
        let empty = HashMap::new();
        let headers = Vec::new();
        let values = RequestBindingValues { path: &empty, query: &query, headers: &headers, cookies: &empty, body: None, form: &empty };
        let binding = binding(RouteBindingSource::Query, "limit", "limit", 0, Some("Int"));
        let error = bind_request_arguments(&[binding], 1, &values).unwrap_err();
        assert_eq!(error.kind, RequestDecodeErrorKind::InvalidType);
        assert_eq!(error.source, Some(RouteBindingSource::Query));
        assert_eq!(error.source_name.as_deref(), Some("limit"));
        assert_eq!(error.expected_type.as_deref(), Some("Int"));
    }

    #[test]
    fn request_target_splits_path_from_query_and_decodes_values() {
        let (path, query) = split_request_target("/users/42?limit=25&term=hello+world&tag=a%2Fb");
        assert_eq!(path, "/users/42");
        assert_eq!(query.get("limit").map(String::as_str), Some("25"));
        assert_eq!(query.get("term").map(String::as_str), Some("hello world"));
        assert_eq!(query.get("tag").map(String::as_str), Some("a/b"));
    }

    #[test]
    fn parses_cookie_header() {
        let cookies = parse_cookie_header("session=abc; theme=dark%20mode");
        assert_eq!(cookies.get("session").map(String::as_str), Some("abc"));
        assert_eq!(cookies.get("theme").map(String::as_str), Some("dark mode"));
    }
}
