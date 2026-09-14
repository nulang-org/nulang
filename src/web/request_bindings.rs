//! Transport-neutral request extraction for Web Contract IR bindings.

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
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(&self.message)
    }
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
            message: format!("route handler has {handler_param_count} parameters; VM ABI supports at most {}", u8::MAX),
        });
    }

    let mut slots = vec![None; handler_param_count];
    for binding in bindings {
        if binding.handler_index >= handler_param_count {
            return Err(error(binding, RequestDecodeErrorKind::InvalidHandlerSlot,
                format!("binding for '{}' targets invalid handler slot {}", binding.handler_param, binding.handler_index)));
        }
        if slots[binding.handler_index].is_some() {
            return Err(error(binding, RequestDecodeErrorKind::DuplicateHandlerSlot,
                format!("multiple request bindings target handler slot {}", binding.handler_index)));
        }

        let raw = lookup(binding, values).ok_or_else(|| error(
            binding,
            RequestDecodeErrorKind::Missing,
            format!("missing {} input '{}' for handler parameter '{}'", source_name(binding.source), binding.source_name, binding.handler_param),
        ))?;
        let value = decode_scalar_constant(raw, binding.ty.as_deref()).map_err(|message| error(
            binding,
            RequestDecodeErrorKind::InvalidType,
            format!("{} input '{}' for handler parameter '{}': {message}", source_name(binding.source), binding.source_name, binding.handler_param),
        ))?;
        slots[binding.handler_index] = Some(BoundRouteArgument {
            handler_index: binding.handler_index,
            value,
        });
    }

    slots.into_iter().enumerate().map(|(index, slot)| slot.ok_or_else(|| RequestDecodeError {
        kind: RequestDecodeErrorKind::IncompleteBindingPlan,
        source: None,
        source_name: None,
        handler_param: None,
        handler_index: Some(index),
        expected_type: None,
        message: format!("handler parameter slot {index} has no request binding"),
    })).collect()
}

fn error(binding: &RouteBindingContract, kind: RequestDecodeErrorKind, message: String) -> RequestDecodeError {
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

fn lookup<'a>(binding: &RouteBindingContract, values: &'a RequestBindingValues<'a>) -> Option<&'a str> {
    match binding.source {
        RouteBindingSource::Path => values.path.get(&binding.source_name).map(String::as_str),
        RouteBindingSource::Query => values.query.get(&binding.source_name).map(String::as_str),
        RouteBindingSource::Header => values.headers.iter()
            .find(|(name, _)| name.eq_ignore_ascii_case(&binding.source_name))
            .map(|(_, value)| value.as_str()),
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
    let mut out = HashMap::new();
    for part in String::from_utf8_lossy(input).split('&') {
        if part.is_empty() { continue; }
        let (key, value) = part.split_once('=').unwrap_or((part, ""));
        out.insert(percent_decode(key), percent_decode(value));
    }
    out
}

pub fn parse_cookie_header(header: &str) -> HashMap<String, String> {
    header.split(';').filter_map(|part| {
        let (key, value) = part.trim().split_once('=')?;
        Some((key.trim().to_string(), percent_decode(value.trim())))
    }).collect()
}

fn percent_decode(input: &str) -> String {
    let bytes = input.as_bytes();
    let mut out = String::with_capacity(input.len());
    let mut i = 0;
    while i < bytes.len() {
        if bytes[i] == b'+' { out.push(' '); i += 1; continue; }
        if bytes[i] == b'%' && i + 2 < bytes.len() {
            if let (Some(a), Some(b)) = (hex(bytes[i + 1]), hex(bytes[i + 2])) {
                out.push(((a << 4) | b) as char); i += 3; continue;
            }
        }
        out.push(bytes[i] as char); i += 1;
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

    fn binding(source: RouteBindingSource, source_name: &str, index: usize, ty: &str) -> RouteBindingContract {
        RouteBindingContract {
            source,
            source_name: source_name.into(),
            handler_param: source_name.into(),
            handler_index: index,
            ty: Some(ty.into()),
        }
    }

    #[test]
    fn binds_all_request_sources() {
        let path = HashMap::from([("id".into(), "42".into())]);
        let query = HashMap::from([("limit".into(), "25".into())]);
        let headers = vec![("X-Trace".into(), "abc".into())];
        let cookies = HashMap::from([("session".into(), "s1".into())]);
        let form = HashMap::from([("title".into(), "hello".into())]);
        let values = RequestBindingValues { path: &path, query: &query, headers: &headers, cookies: &cookies, body: Some("payload"), form: &form };
        let bindings = vec![
            binding(RouteBindingSource::Path, "id", 0, "Int"),
            binding(RouteBindingSource::Query, "limit", 1, "Int"),
            binding(RouteBindingSource::Header, "x-trace", 2, "String"),
            binding(RouteBindingSource::Cookie, "session", 3, "String"),
            binding(RouteBindingSource::Body, "body", 4, "String"),
            binding(RouteBindingSource::Form, "title", 5, "String"),
        ];
        let args = bind_request_arguments(&bindings, 6, &values).unwrap();
        assert_eq!(args[0].value, Constant::Int(42));
        assert_eq!(args[1].value, Constant::Int(25));
        assert_eq!(args[2].value, Constant::String("abc".into()));
    }

    #[test]
    fn invalid_scalar_returns_structured_error() {
        let empty = HashMap::new();
        let query = HashMap::from([("limit".into(), "many".into())]);
        let headers = Vec::new();
        let values = RequestBindingValues { path: &empty, query: &query, headers: &headers, cookies: &empty, body: None, form: &empty };
        let err = bind_request_arguments(&[binding(RouteBindingSource::Query, "limit", 0, "Int")], 1, &values).unwrap_err();
        assert_eq!(err.kind, RequestDecodeErrorKind::InvalidType);
        assert_eq!(err.source, Some(RouteBindingSource::Query));
        assert_eq!(err.expected_type.as_deref(), Some("Int"));
    }

    #[test]
    fn parses_target_form_and_cookies() {
        let (path, query) = split_request_target("/users?term=hello+world&tag=a%2Fb");
        assert_eq!(path, "/users");
        assert_eq!(query.get("term").map(String::as_str), Some("hello world"));
        assert_eq!(query.get("tag").map(String::as_str), Some("a/b"));
        assert_eq!(parse_urlencoded(b"title=hello+world").get("title").map(String::as_str), Some("hello world"));
        assert_eq!(parse_cookie_header("session=abc; theme=dark%20mode").get("theme").map(String::as_str), Some("dark mode"));
    }
}
