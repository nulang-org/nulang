//! OpenAPI generation from the compiler-owned Nulang Web Contract IR.
//!
//! This module deliberately consumes [`ContractCompilation`] rather than
//! reparsing source. Runtime dispatch, deployment metadata, OpenAPI, and future
//! client generators therefore describe the same validated route contracts.

use crate::web::contracts::{ContractCompilation, RouteContract, RouteParamContract};
use serde_json::{json, Map, Value};

pub const OPENAPI_VERSION: &str = "3.1.0";

/// Generate an OpenAPI 3.1 document from a validated Web Contract compilation.
///
/// Response media types are intentionally not invented here. The current Web
/// runtime still has HTML-oriented response behavior and the typed response
/// algebra (`Json[T]`, `Html`, `Stream[T]`, etc.) has not landed yet. We retain
/// the Nulang response/error type as extensions until that contract is explicit.
pub fn generate_openapi(
    contracts: &ContractCompilation,
    title: &str,
    version: &str,
) -> Value {
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
    let parameters: Vec<Value> = route.params.iter().map(path_parameter).collect();
    let operation_id = route.handler.clone().unwrap_or_else(|| {
        let suffix = route
            .path
            .trim_matches('/')
            .replace(['/', ':', '{', '}', ' '], "_");
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
    operation.insert("responses".to_string(), Value::Object(responses));

    if !route.effects.is_empty() {
        operation.insert(
            "x-nulang-effects".to_string(),
            Value::Array(
                route
                    .effects
                    .iter()
                    .cloned()
                    .map(Value::String)
                    .collect(),
            ),
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
            }],
            response_type: Some("User".to_string()),
            error_type: Some("UserError".to_string()),
            effects: vec!["DB".to_string()],
            reference_capability: None,
            placement: Some("server".to_string()),
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
    }

    #[test]
    fn maps_primitive_path_types_to_openapi_schema() {
        assert_eq!(schema_for_type(Some("Int"))["type"], "integer");
        assert_eq!(schema_for_type(Some("Float"))["type"], "number");
        assert_eq!(schema_for_type(Some("Bool"))["type"], "boolean");
        assert_eq!(schema_for_type(Some("String"))["type"], "string");
    }
}
