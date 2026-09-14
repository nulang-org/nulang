//! OpenAPI generation from the compiler-owned Nulang Web Contract IR.
//!
//! This module deliberately consumes [`ContractCompilation`] rather than
//! reparsing source. Runtime dispatch, deployment metadata, OpenAPI, and future
//! client generators therefore describe the same validated route contracts.

use crate::web::bindings::{compile_route_bindings, RouteBindingContract, RouteBindingSource};
use crate::web::contracts::{ContractCompilation, RouteContract, RouteParamContract};
use serde_json::{json, Map, Value};

pub const OPENAPI_VERSION: &str = "3.1.0";

/// Generate an OpenAPI 3.1 document from a validated Web Contract compilation.
///
/// Response media types are intentionally not invented here. The current Web
/// runtime still has HTML-oriented response behavior and the typed response
/// algebra (`Json[T]`, `Html`, `Stream[T]`, etc.) has not landed yet. We retain
/// the Nulang response/error type as extensions until that contract is explicit.
pub fn generate_openapi(contracts: &ContractCompilation, title: &str, version: &str) -> Value {
    let mut paths = Map::new();

    for route in &contracts.routes {
        let path = openapi_path(&route.path);
        let method = route.method.to_ascii_lowercase();
        let operation = operation_for(route);

        let entry = paths
            .entry(path)
            .or_insert_with(|| Value::Object(Map::new()));
        if let Value::Object(methods) = entry {
            methods.insert(method, operation);
        }
    }

    json!({
        "openapi": OPENAPI_VERSION,
        "info": {
            "title": title,
            "version": version,
        },
        "paths": Value::Object(paths),
        "x-nulang-contract-version": 1,
    })
}

fn operation_for(route: &RouteContract) -> Value {
    // Path parameters continue to come directly from the route contract so
    // legacy routes that still use ambient `Web.param` remain accurately
    // described. Additional request parameters come from compiler binding IR.
    let mut parameters: Vec<Value> = route.params.iter().map(path_parameter).collect();
    let binding_compilation = compile_route_bindings(route);
    parameters.extend(
        binding_compilation
            .bindings
            .iter()
            .filter_map(binding_parameter),
    );

    let operation_id = route.handler.clone().unwrap_or_else(|| {
        let suffix = route
            .path
            .trim_matches('/')
            .replace(|c: char| matches!(c, '/' | ':' | '{' | '}' | ' '), "_");
        format!("{}_{}", route.method.to_ascii_lowercase(), suffix)
            .trim_end_matches('_')
            .to_string()
    });

    let mut success = Map::new();
    success.insert(
        "description".to_string(),
        Value::String("Successful response".to_string()),
    );
    if let Some(response_type) = &route.response_type {
        success.insert(
            "x-nulang-response-type".to_string(),
            Value::String(response_type.clone()),
        );
    }

    let mut responses = Map::new();
    responses.insert("200".to_string(), Value::Object(success));
    if let Some(error_type) = &route.error_type {
        responses.insert(
            "default".to_string(),
            json!({
                "description": "Typed error response",
                "x-nulang-error-type": error_type,
            }),
        );
    }

    let mut operation = Map::new();
    operation.insert("operationId".to_string(), Value::String(operation_id));
    if !parameters.is_empty() {
        operation.insert("parameters".to_string(), Value::Array(parameters));
    }
    if let Some(request_body) = form_request_body(&binding_compilation.bindings) {
        operation.insert("requestBody".to_string(), request_body);
    }
    operation.insert("responses".to_string(), Value::Object(responses));

    if !binding_compilation.bindings.is_empty() {
        operation.insert(
            "x-nulang-request-bindings".to_string(),
            Value::Array(
                binding_compilation
                    .bindings
                    .iter()
                    .map(binding_extension)
                    .collect(),
            ),
        );
    }
    if !binding_compilation.diagnostics.is_empty() {
        operation.insert(
            "x-nulang-binding-diagnostics".to_string(),
            Value::Array(
                binding_compilation
                    .diagnostics
                    .iter()
                    .cloned()
                    .map(Value::String)
                    .collect(),
            ),
        );
    }
    if !route.effects.is_empty() {
        operation.insert(
            "x-nulang-effects".to_string(),
            Value::Array(route.effects.iter().cloned().map(Value::String).collect()),
        );
    }
    if let Some(placement) = &route.placement {
        operation.insert(
            "x-nulang-placement".to_string(),
            Value::String(placement.clone()),
        );
    }
    if let Some(capability) = &route.reference_capability {
        operation.insert(
            "x-nulang-reference-capability".to_string(),
            Value::String(capability.clone()),
        );
    }

    Value::Object(operation)
}

fn path_parameter(param: &RouteParamContract) -> Value {
    json!({
        "name": param.name,
        "in": "path",
        "required": true,
        "schema": schema_for_type(param.ty.as_deref()),
    })
}

/// Convert request bindings that OpenAPI represents as parameters. Raw body
/// bindings intentionally remain in `x-nulang-request-bindings` until Nulang's
/// request algebra defines a media type. Form bindings are represented through
/// an URL-encoded `requestBody` because their transport semantics are explicit.
fn binding_parameter(binding: &RouteBindingContract) -> Option<Value> {
    let location = match binding.source {
        RouteBindingSource::Query => "query",
        RouteBindingSource::Header => "header",
        RouteBindingSource::Cookie => "cookie",
        RouteBindingSource::Path | RouteBindingSource::Body | RouteBindingSource::Form => {
            return None
        }
    };

    Some(json!({
        "name": binding.source_name,
        "in": location,
        "required": true,
        "schema": schema_for_type(binding.ty.as_deref()),
        "x-nulang-handler-param": binding.handler_param,
    }))
}

/// Generate a real OpenAPI request body for `from form(...)` bindings.
///
/// The HTTP runtime only populates form bindings for
/// `application/x-www-form-urlencoded`, so this media type is compiler-owned
/// behavior rather than a guess made by the documentation generator.
fn form_request_body(bindings: &[RouteBindingContract]) -> Option<Value> {
    let form_bindings: Vec<_> = bindings
        .iter()
        .filter(|binding| binding.source == RouteBindingSource::Form)
        .collect();
    if form_bindings.is_empty() {
        return None;
    }

    let mut properties = Map::new();
    let mut required = Vec::new();
    for binding in form_bindings {
        let mut schema = schema_for_type(binding.ty.as_deref());
        if let Value::Object(fields) = &mut schema {
            fields.insert(
                "x-nulang-handler-param".to_string(),
                Value::String(binding.handler_param.clone()),
            );
        }
        properties.insert(binding.source_name.clone(), schema);
        if !required.iter().any(|name| name == &binding.source_name) {
            required.push(binding.source_name.clone());
        }
    }

    Some(json!({
        "required": true,
        "content": {
            "application/x-www-form-urlencoded": {
                "schema": {
                    "type": "object",
                    "properties": Value::Object(properties),
                    "required": required,
                }
            }
        }
    }))
}

fn binding_extension(binding: &RouteBindingContract) -> Value {
    json!({
        "source": binding_source_name(binding.source),
        "source_name": binding.source_name,
        "handler_param": binding.handler_param,
        "handler_index": binding.handler_index,
        "type": binding.ty,
    })
}

fn binding_source_name(source: RouteBindingSource) -> &'static str {
    match source {
        RouteBindingSource::Path => "path",
        RouteBindingSource::Query => "query",
        RouteBindingSource::Header => "header",
        RouteBindingSource::Cookie => "cookie",
        RouteBindingSource::Body => "body",
        RouteBindingSource::Form => "form",
    }
}

fn schema_for_type(ty: Option<&str>) -> Value {
    match ty.map(str::trim) {
        Some("Int") => json!({ "type": "integer", "format": "int64" }),
        Some("Float") => json!({ "type": "number", "format": "double" }),
        Some("Bool") => json!({ "type": "boolean" }),
        Some("String") | None => json!({ "type": "string" }),
        Some(other) => json!({
            "type": "string",
            "x-nulang-type": other,
        }),
    }
}

/// Convert Nulang route syntax to OpenAPI path-template syntax.
///
/// - `/users/:id` -> `/users/{id}`
/// - `/users/{id}` -> unchanged
/// - `/users/{id: UserId}` -> `/users/{id}`
pub fn openapi_path(path: &str) -> String {
    let leading_slash = path.starts_with('/');
    let segments: Vec<String> = path
        .trim_start_matches('/')
        .split('/')
        .map(|segment| {
            if let Some(name) = segment.strip_prefix(':') {
                return format!("{{{name}}}");
            }
            if let Some(inner) = segment
                .strip_prefix('{')
                .and_then(|value| value.strip_suffix('}'))
            {
                let name = inner.split_once(':').map_or(inner, |(name, _)| name).trim();
                return format!("{{{name}}}");
            }
            segment.to_string()
        })
        .collect();

    let joined = segments.join("/");
    if leading_slash {
        format!("/{joined}")
    } else {
        joined
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::web::contracts::{HandlerParamContract, RouteParamContract};

    fn route() -> RouteContract {
        RouteContract {
            method: "GET".to_string(),
            path: "/users/{id: UserId}".to_string(),
            handler: Some("show_user".to_string()),
            params: vec![RouteParamContract {
                name: "id".to_string(),
                ty: Some("UserId".to_string()),
            }],
            handler_params: vec![HandlerParamContract {
                name: "id".to_string(),
                ty: Some("UserId".to_string()),
                capability: None,
                request: None,
            }],
            response_type: Some("User".to_string()),
            error_type: Some("UserError".to_string()),
            effects: vec!["DB".to_string()],
            reference_capability: None,
            placement: Some("server".to_string()),
        }
    }

    fn binding(source: RouteBindingSource, name: &str, ty: &str) -> RouteBindingContract {
        RouteBindingContract {
            source,
            source_name: name.to_string(),
            handler_param: format!("handler_{name}"),
            handler_index: 0,
            ty: Some(ty.to_string()),
        }
    }

    #[test]
    fn normalizes_route_templates() {
        assert_eq!(openapi_path("/users/:id"), "/users/{id}");
        assert_eq!(
            openapi_path("/orgs/{org: OrgId}/users/{user}"),
            "/orgs/{org}/users/{user}"
        );
    }

    #[test]
    fn emits_contract_metadata_without_inventing_response_media_type() {
        let document = generate_openapi(
            &ContractCompilation {
                routes: vec![route()],
                diagnostics: Vec::new(),
            },
            "Example API",
            "1.0.0",
        );

        assert_eq!(document["openapi"], OPENAPI_VERSION);
        let operation = &document["paths"]["/users/{id}"]["get"];
        assert_eq!(operation["operationId"], "show_user");
        assert_eq!(operation["parameters"][0]["name"], "id");
        assert_eq!(operation["parameters"][0]["in"], "path");
        assert_eq!(
            operation["parameters"][0]["schema"]["x-nulang-type"],
            "UserId"
        );
        assert_eq!(
            operation["responses"]["200"]["x-nulang-response-type"],
            "User"
        );
        assert_eq!(
            operation["responses"]["default"]["x-nulang-error-type"],
            "UserError"
        );
        assert!(operation["responses"]["200"].get("content").is_none());
        assert_eq!(operation["x-nulang-placement"], "server");
        assert_eq!(operation["x-nulang-effects"][0], "DB");
        assert_eq!(operation["x-nulang-request-bindings"][0]["source"], "path");
    }

    #[test]
    fn maps_non_path_bindings_to_openapi_parameters() {
        let query = binding(RouteBindingSource::Query, "limit", "Int");
        let header = binding(RouteBindingSource::Header, "X-Trace", "String");
        let cookie = binding(RouteBindingSource::Cookie, "session", "String");
        let body = binding(RouteBindingSource::Body, "body", "Payload");

        let query_param = binding_parameter(&query).unwrap();
        assert_eq!(query_param["in"], "query");
        assert_eq!(query_param["schema"]["type"], "integer");
        assert_eq!(binding_parameter(&header).unwrap()["in"], "header");
        assert_eq!(binding_parameter(&cookie).unwrap()["in"], "cookie");
        assert!(binding_parameter(&body).is_none());
        assert_eq!(binding_extension(&body)["source"], "body");
    }

    #[test]
    fn emits_urlencoded_form_bindings_as_request_body() {
        let bindings = vec![
            binding(RouteBindingSource::Form, "title", "String"),
            binding(RouteBindingSource::Form, "count", "Int"),
        ];
        let body = form_request_body(&bindings).unwrap();

        let schema = &body["content"]["application/x-www-form-urlencoded"]["schema"];
        assert_eq!(body["required"], true);
        assert_eq!(schema["type"], "object");
        assert_eq!(schema["properties"]["title"]["type"], "string");
        assert_eq!(schema["properties"]["count"]["type"], "integer");
        assert_eq!(schema["required"][0], "title");
        assert_eq!(schema["required"][1], "count");
    }

    #[test]
    fn raw_body_does_not_invent_an_openapi_media_type() {
        let body = binding(RouteBindingSource::Body, "body", "Payload");
        assert!(form_request_body(&[body]).is_none());
    }

    #[test]
    fn maps_primitive_path_types_to_openapi_schema() {
        assert_eq!(schema_for_type(Some("Int"))["type"], "integer");
        assert_eq!(schema_for_type(Some("Float"))["type"], "number");
        assert_eq!(schema_for_type(Some("Bool"))["type"], "boolean");
        assert_eq!(schema_for_type(Some("String"))["type"], "string");
    }
}
