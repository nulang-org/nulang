//! Compiler-owned request/response body codec metadata.
//!
//! Codecs describe wire semantics without changing Nulang's VM value model.
//! `Json[T]` currently erases to an already-serialized `String`; the codec
//! validates the wire representation and preserves `T` as schema metadata.

use serde::{Deserialize, Serialize};
use serde_json::Value;

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum BodyCodecKind {
    Json,
}

impl BodyCodecKind {
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Json => "json",
        }
    }
}

/// Transport-level JSON schema that is safe to derive from compiler-known
/// Nulang types. Unknown/unsupported type constructors are left unmodelled
/// rather than guessed.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "kind", rename_all = "snake_case")]
pub enum JsonSchemaContract {
    String,
    Bool,
    Int,
    Float,
    Array {
        items: Box<JsonSchemaContract>,
    },
    Object {
        #[serde(default, skip_serializing_if = "Option::is_none")]
        type_name: Option<String>,
        fields: Vec<JsonSchemaFieldContract>,
    },
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct JsonSchemaFieldContract {
    pub name: String,
    pub schema: JsonSchemaContract,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct BodyCodecContract {
    pub codec: BodyCodecKind,
    pub media_type: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub payload_type: Option<String>,
    /// Structural schema proven by the compiler, when the payload type is
    /// sufficiently known. Absence means syntax/media validation only.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub schema: Option<JsonSchemaContract>,
}

impl BodyCodecContract {
    pub fn json(payload_type: String) -> Self {
        let schema = builtin_json_schema(&payload_type);
        Self {
            codec: BodyCodecKind::Json,
            media_type: "application/json".to_string(),
            payload_type: Some(payload_type),
            schema,
        }
    }

    pub fn with_schema(mut self, schema: Option<JsonSchemaContract>) -> Self {
        if schema.is_some() {
            self.schema = schema;
        }
        self
    }
}

pub fn builtin_json_schema(payload_type: &str) -> Option<JsonSchemaContract> {
    match payload_type.trim() {
        "String" => Some(JsonSchemaContract::String),
        "Bool" => Some(JsonSchemaContract::Bool),
        "Int" => Some(JsonSchemaContract::Int),
        "Float" => Some(JsonSchemaContract::Float),
        _ => None,
    }
}

/// Derive an explicit wire codec from a source-level body type.
///
/// Unknown/unwrapped types deliberately return `None`: protocol semantics are
/// never guessed from arbitrary domain names or from `String`.
pub fn body_codec_for_type(ty: Option<&str>) -> Option<BodyCodecContract> {
    let ty = ty?.trim();
    let payload = ty.strip_prefix("Json[")?.strip_suffix(']')?.trim();
    if payload.is_empty() {
        return None;
    }
    Some(BodyCodecContract::json(payload.to_string()))
}

/// Validate an encoded body according to its compiler-owned codec.
///
/// The runtime representation remains the original serialized text. This
/// validator proves transport correctness without depending on VM record layout.
/// Compiler-proven record schemas validate required fields recursively while
/// still allowing extra JSON object fields for forward-compatible clients.
pub fn validate_encoded_body(contract: &BodyCodecContract, raw: &str) -> Result<(), String> {
    match contract.codec {
        BodyCodecKind::Json => validate_json(contract, raw),
    }
}

fn validate_json(contract: &BodyCodecContract, raw: &str) -> Result<(), String> {
    let value: Value =
        serde_json::from_str(raw).map_err(|error| format!("invalid JSON body: {error}"))?;

    if let Some(schema) = &contract.schema {
        return validate_json_value(schema, &value, "$");
    }

    // Backwards-compatible fallback for codec metadata produced before
    // structural schemas are attached.
    let valid_shape = match contract.payload_type.as_deref().map(str::trim) {
        Some("String") => value.is_string(),
        Some("Bool") => value.is_boolean(),
        Some("Int") => value.as_i64().is_some() || value.as_u64().is_some(),
        Some("Float") => value.as_f64().is_some(),
        _ => true,
    };

    if valid_shape {
        Ok(())
    } else {
        Err(format!(
            "JSON body does not match payload type {}",
            contract.payload_type.as_deref().unwrap_or("<unknown>")
        ))
    }
}

fn validate_json_value(
    schema: &JsonSchemaContract,
    value: &Value,
    path: &str,
) -> Result<(), String> {
    match schema {
        JsonSchemaContract::String if value.is_string() => Ok(()),
        JsonSchemaContract::Bool if value.is_boolean() => Ok(()),
        JsonSchemaContract::Int if value.as_i64().is_some() || value.as_u64().is_some() => Ok(()),
        JsonSchemaContract::Float if value.as_f64().is_some() => Ok(()),
        JsonSchemaContract::Array { items } => {
            let values = value
                .as_array()
                .ok_or_else(|| format!("{path} must be a JSON array"))?;
            for (index, item) in values.iter().enumerate() {
                validate_json_value(items, item, &format!("{path}[{index}]"))?;
            }
            Ok(())
        }
        JsonSchemaContract::Object { fields, .. } => {
            let object = value
                .as_object()
                .ok_or_else(|| format!("{path} must be a JSON object"))?;
            for field in fields {
                let field_path = format!("{path}.{}", field.name);
                let field_value = object
                    .get(&field.name)
                    .ok_or_else(|| format!("missing required JSON field {field_path}"))?;
                validate_json_value(&field.schema, field_value, &field_path)?;
            }
            Ok(())
        }
        JsonSchemaContract::String => Err(format!("{path} must be a JSON string")),
        JsonSchemaContract::Bool => Err(format!("{path} must be a JSON boolean")),
        JsonSchemaContract::Int => Err(format!("{path} must be a JSON integer")),
        JsonSchemaContract::Float => Err(format!("{path} must be a JSON number")),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn derives_json_codec_without_guessing_string() {
        let codec = body_codec_for_type(Some("Json[CreateUser]")).unwrap();
        assert_eq!(codec.codec, BodyCodecKind::Json);
        assert_eq!(codec.media_type, "application/json");
        assert_eq!(codec.payload_type.as_deref(), Some("CreateUser"));
        assert!(codec.schema.is_none());

        assert!(body_codec_for_type(Some("String")).is_none());
        assert!(body_codec_for_type(Some("CreateUser")).is_none());
        assert!(body_codec_for_type(Some("Json[]")).is_none());
    }

    #[test]
    fn validates_json_syntax_and_primitive_shape() {
        let domain = body_codec_for_type(Some("Json[CreateUser]")).unwrap();
        assert!(validate_encoded_body(&domain, r#"{"name":"Ada"}"#).is_ok());
        assert!(validate_encoded_body(&domain, "{").is_err());

        let string = body_codec_for_type(Some("Json[String]")).unwrap();
        assert!(validate_encoded_body(&string, r#""hello""#).is_ok());
        assert!(validate_encoded_body(&string, "42").is_err());

        let int = body_codec_for_type(Some("Json[Int]")).unwrap();
        assert!(validate_encoded_body(&int, "42").is_ok());
        assert!(validate_encoded_body(&int, "42.5").is_err());
    }

    #[test]
    fn validates_compiler_proven_record_schema_recursively() {
        let schema = JsonSchemaContract::Object {
            type_name: Some("CreateUser".to_string()),
            fields: vec![
                JsonSchemaFieldContract {
                    name: "name".to_string(),
                    schema: JsonSchemaContract::String,
                },
                JsonSchemaFieldContract {
                    name: "scores".to_string(),
                    schema: JsonSchemaContract::Array {
                        items: Box::new(JsonSchemaContract::Int),
                    },
                },
            ],
        };
        let codec = BodyCodecContract::json("CreateUser".to_string()).with_schema(Some(schema));

        assert!(validate_encoded_body(
            &codec,
            r#"{"name":"Ada","scores":[1,2],"extra":true}"#
        )
        .is_ok());
        assert!(validate_encoded_body(&codec, r#"{"scores":[1,2]}"#)
            .unwrap_err()
            .contains("$.name"));
        assert!(validate_encoded_body(&codec, r#"{"name":"Ada","scores":[1,2.5]}"#)
            .unwrap_err()
            .contains("$.scores[1]"));
    }
}
