//! `ToolSchema`: JSON-schema description of an `@tool`-annotated function.
//!
//! Lives in core (unconditional, no `ai-runtime` gate) because it is
//! embedded in core, always-compiled types: `bytecode::ActorMeta.tools`
//! and `hir::Decl`'s agent `tools` field. The `@tool` annotation is
//! permanent language surface (RFC 0010 §C.2: "Keep as-is; document
//! extensibility") independent of whether any LLM client is compiled in.
//!
//! This is a distinct type from `nulang_ai::request::ToolSchema` (used for
//! the actual LLM-provider wire format in `LlmRequest.tools`), which stays
//! in the optional `nulang-ai` crate per its "zero dependency on core"
//! charter. `runtime/agent.rs` converts between the two, behind
//! `ai-runtime`, at the one point a tool schema is handed to a provider.

use serde::{Deserialize, Serialize};
use serde_json::Map;

use crate::types::{PrimitiveType, Type};

/// JSON-schema description of a tool exposed to an LLM (or any future
/// caller of `@tool`-annotated functions).
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct ToolSchema {
    /// Tool name.
    pub name: String,
    /// Human-readable description.
    pub description: String,
    /// JSON schema for the tool arguments.
    pub parameters: serde_json::Value,
}

/// Convert a Nulang type into a JSON Schema value.
///
/// Unknown or unresolved types map to the unsatisfiable JSON Schema `false`
/// rather than the unconstrained schema `{}`. This is particularly important
/// for tool schemas exposed to external LLM providers.
pub fn type_to_json_schema(ty: &Type) -> serde_json::Value {
    checked_type_to_json_schema(ty).unwrap_or_else(|_| serde_json::json!(false))
}

/// Checked JSON Schema conversion for compiler diagnostics and public APIs.
///
/// Every nested member must be representable. A partial schema is unsafe:
/// replacing an unknown field type with `{}` would accept arbitrary input.
pub fn checked_type_to_json_schema(ty: &Type) -> Result<serde_json::Value, String> {
    match ty {
        Type::Var(_) => Err("unresolved type variable".to_string()),
        Type::Primitive(PrimitiveType::Int) => Ok(serde_json::json!({"type": "integer"})),
        Type::Primitive(PrimitiveType::Float) => Ok(serde_json::json!({"type": "number"})),
        Type::Primitive(PrimitiveType::Bool) => Ok(serde_json::json!({"type": "boolean"})),
        Type::Primitive(PrimitiveType::String) => Ok(serde_json::json!({"type": "string"})),
        Type::Primitive(PrimitiveType::Unit | PrimitiveType::Nil) => {
            Ok(serde_json::json!({"type": "null"}))
        }
        Type::Primitive(PrimitiveType::Never | PrimitiveType::Address) => {
            Err("type is not a JSON value".to_string())
        }
        Type::Record(fields) => {
            let mut properties = Map::new();
            let mut required = Vec::with_capacity(fields.len());
            for (name, field_ty) in fields {
                if name == ".." {
                    return Err("open record rows cannot be represented as a closed schema".into());
                }
                if properties.contains_key(name) {
                    return Err(format!("duplicate record field: {name}"));
                }
                let schema = checked_type_to_json_schema(field_ty)
                    .map_err(|err| format!("record field {name}: {err}"))?;
                properties.insert(name.clone(), schema);
                required.push(name.clone());
            }
            Ok(serde_json::json!({
                "type": "object",
                "properties": properties,
                "required": required,
                "additionalProperties": false,
            }))
        }
        Type::Array(inner) => Ok(serde_json::json!({
            "type": "array",
            "items": checked_type_to_json_schema(inner)?,
        })),
        Type::Tuple(elems) => {
            let schemas: Result<Vec<_>, _> = elems
                .iter()
                .enumerate()
                .map(|(i, ty)| {
                    checked_type_to_json_schema(ty)
                        .map_err(|err| format!("tuple element {i}: {err}"))
                })
                .collect();
            Ok(serde_json::json!({
                "type": "array",
                "prefixItems": schemas?,
                "items": false,
                "minItems": elems.len(),
            }))
        }
        Type::Variant(variants) => {
            if variants.is_empty() {
                return Err("empty variant has no JSON representation".into());
            }
            let mut names = std::collections::HashSet::new();
            let mut schemas = Vec::with_capacity(variants.len());
            for (name, inner) in variants {
                if !names.insert(name) {
                    return Err(format!("duplicate variant case: {name}"));
                }
                let schema = if let Some(inner_ty) = inner {
                    serde_json::json!({
                        "type": "object",
                        "properties": {
                            name: checked_type_to_json_schema(inner_ty)
                                .map_err(|err| format!("variant {name}: {err}"))?,
                        },
                        "required": [name],
                        "additionalProperties": false,
                    })
                } else {
                    serde_json::json!({"type": "string", "enum": [name]})
                };
                schemas.push(schema);
            }
            Ok(serde_json::json!({"oneOf": schemas}))
        }
        Type::Reference { inner, .. } | Type::Scheme { body: inner, .. } => {
            checked_type_to_json_schema(inner)
        }
        Type::Nominal { underlying, .. } => checked_type_to_json_schema(underlying),
        Type::App { constructor, args } => {
            let name = match constructor.as_ref() {
                Type::Nominal { name, .. } => name.as_str(),
                _ => return Err("unrecognized generic type constructor".into()),
            };
            if args.len() != 1 {
                return Err(format!("{name} expects one type parameter"));
            }
            match name {
                "List" | "Array" => Ok(serde_json::json!({
                    "type": "array",
                    "items": checked_type_to_json_schema(&args[0])?,
                })),
                "Option" => Ok(serde_json::json!({
                    "anyOf": [
                        {"type": "null"},
                        checked_type_to_json_schema(&args[0])?,
                    ],
                })),
                _ => Err(format!("unsupported generic type constructor: {name}")),
            }
        }
        Type::Function { .. } | Type::Actor { .. } | Type::Skolem(_) => {
            Err("non-serializable or unresolved type".into())
        }
    }
}

/// Build a tool schema. Unsupported parameters produce an unsatisfiable
/// schema instead of exposing unconstrained input to a provider. Use the
/// checked variant below when the caller can surface a compiler diagnostic.
pub fn function_to_tool_schema(
    name: &str,
    description: &str,
    params: &[(String, Type)],
    ret: &Type,
) -> ToolSchema {
    try_function_to_tool_schema(name, description, params, ret).unwrap_or_else(|_| ToolSchema {
        name: name.to_string(),
        description: description.to_string(),
        parameters: serde_json::json!(false),
    })
}

/// Build a schema with explicit diagnostics on invalid tool parameter types.
/// Return types are not represented in the provider's argument schema.
pub fn try_function_to_tool_schema(
    name: &str,
    description: &str,
    params: &[(String, Type)],
    _ret: &Type,
) -> Result<ToolSchema, String> {
    let mut properties = Map::new();
    let mut required = Vec::with_capacity(params.len());
    for (param_name, param_ty) in params {
        if properties.contains_key(param_name) {
            return Err(format!("duplicate tool parameter: {param_name}"));
        }
        let schema = checked_type_to_json_schema(param_ty)
            .map_err(|err| format!("parameter {param_name}: {err}"))?;
        properties.insert(param_name.clone(), schema);
        required.push(param_name.clone());
    }

    Ok(ToolSchema {
        name: name.to_string(),
        description: description.to_string(),
        parameters: serde_json::json!({
            "type": "object",
            "properties": properties,
            "required": required,
            "additionalProperties": false,
        }),
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_primitive_schemas() {
        assert_eq!(
            type_to_json_schema(&Type::Primitive(PrimitiveType::Int)),
            serde_json::json!({"type": "integer"})
        );
        assert_eq!(
            type_to_json_schema(&Type::Primitive(PrimitiveType::String)),
            serde_json::json!({"type": "string"})
        );
    }

    #[test]
    fn test_function_to_tool_schema_basic() {
        let params = vec![
            ("city".to_string(), Type::Primitive(PrimitiveType::String)),
            ("days".to_string(), Type::Primitive(PrimitiveType::Int)),
        ];
        let schema = function_to_tool_schema(
            "get_weather",
            "Get the weather forecast",
            &params,
            &Type::Primitive(PrimitiveType::String),
        );
        assert_eq!(schema.name, "get_weather");
        assert_eq!(schema.description, "Get the weather forecast");
        assert_eq!(schema.parameters["type"], "object");
        assert_eq!(
            schema.parameters["required"],
            serde_json::json!(["city", "days"])
        );
    }

    #[test]
    fn test_array_and_option_schemas() {
        let arr = Type::Array(Box::new(Type::Primitive(PrimitiveType::Int)));
        assert_eq!(
            type_to_json_schema(&arr),
            serde_json::json!({"type": "array", "items": {"type": "integer"}})
        );
    }

    #[test]
    fn unknown_types_are_never_unconstrained_schemas() {
        let unresolved = Type::Var(crate::types::TypeVar(999));
        assert_eq!(type_to_json_schema(&unresolved), serde_json::json!(false));
        assert_eq!(
            type_to_json_schema(&Type::Primitive(PrimitiveType::Address)),
            serde_json::json!(false)
        );
    }

    #[test]
    fn unknown_nested_type_rejects_entire_schema() {
        let payload = Type::Record(vec![(
            "items".to_string(),
            Type::Array(Box::new(Type::Var(crate::types::TypeVar(999)))),
        )]);
        assert_eq!(type_to_json_schema(&payload), serde_json::json!(false));
    }

    #[test]
    fn tool_with_unsupported_parameter_is_not_advertised_as_unconstrained() {
        let schema = function_to_tool_schema(
            "unsafe_tool",
            "Not safely serializable",
            &[("payload".to_string(), Type::Var(crate::types::TypeVar(999)))],
            &Type::unit(),
        );
        assert_eq!(schema.parameters, serde_json::json!(false));
    }

    #[test]
    fn supported_records_are_closed_at_schema_boundary() {
        let record = Type::Record(vec![("count".to_string(), Type::int())]);
        assert_eq!(
            type_to_json_schema(&record),
            serde_json::json!({
                "type": "object",
                "properties": {"count": {"type": "integer"}},
                "required": ["count"],
                "additionalProperties": false,
            })
        );
    }

    #[test]
    fn checked_conversion_reports_nested_type_diagnostic() {
        let ty = Type::Record(vec![(
            "payload".to_string(),
            Type::Var(crate::types::TypeVar(999)),
        )]);
        let err = checked_type_to_json_schema(&ty).unwrap_err();
        assert!(err.contains("record field payload"), "{err}");
        assert!(err.contains("unresolved type variable"), "{err}");
    }

    #[test]
    fn generic_option_and_named_variant_fields_have_precise_schemas() {
        let option = Type::App {
            constructor: Box::new(Type::Nominal {
                name: "Option".to_string(),
                underlying: Box::new(Type::unit()),
            }),
            args: vec![Type::int()],
        };
        assert_eq!(
            type_to_json_schema(&option),
            serde_json::json!({
                "anyOf": [{"type": "null"}, {"type": "integer"}]
            })
        );
        let variant = Type::Variant(vec![("Accepted".to_string(), Some(Type::int()))]);
        assert_eq!(
            type_to_json_schema(&variant),
            serde_json::json!({
                "oneOf": [{
                    "type": "object",
                    "properties": {"Accepted": {"type": "integer"}},
                    "required": ["Accepted"],
                    "additionalProperties": false
                }]
            })
        );
    }

    #[test]
    fn duplicate_tool_parameters_fail_closed_with_context() {
        let params = vec![
            ("x".to_string(), Type::int()),
            ("x".to_string(), Type::string()),
        ];
        let err = try_function_to_tool_schema("duplicate", "", &params, &Type::unit()).unwrap_err();
        assert!(err.contains("duplicate tool parameter: x"), "{err}");
        let safe = function_to_tool_schema("duplicate", "", &params, &Type::unit());
        assert_eq!(safe.parameters, serde_json::json!(false));
    }
}
