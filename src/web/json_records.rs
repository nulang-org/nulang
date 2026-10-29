//! Flat, compiler-described JSON record boundary.
//! Nested records, arrays, nullable values and coercion are intentionally
//! unsupported until the web ABI and schema compiler represent them precisely.

use crate::bytecode::Constant;
use serde::de::{MapAccess, Visitor};
use serde::{Deserialize, Deserializer};
use serde_json::Value;
use std::collections::{HashMap, HashSet};
use std::fmt;

/// Supported Web Contract JSON shapes: primitive scalars and nested closed
/// records. Explicit bounds avoid introducing unbounded schema recursion.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum JsonType {
    Scalar(String),
    Record(Vec<(String, JsonType)>),
}

impl JsonType {
    pub fn wire_name(&self) -> String {
        match self {
            Self::Scalar(name) => name.clone(),
            Self::Record(fields) => format!(
                "{{ {} }}",
                fields
                    .iter()
                    .map(|(name, ty)| format!("{name}: {}", ty.wire_name()))
                    .collect::<Vec<_>>()
                    .join(", ")
            ),
        }
    }
}

/// Parser for complete, recursively nested record type strings. Every
/// sub-field must be an explicit supported shape. No implicit Option/null.
pub fn parse_json_type(ty: &str) -> Option<JsonType> {
    fn whitespace(bytes: &[u8], pos: &mut usize) {
        while bytes.get(*pos).is_some_and(u8::is_ascii_whitespace) {
            *pos += 1;
        }
    }
    fn ident<'a>(text: &'a str, pos: &mut usize) -> Option<&'a str> {
        let bytes = text.as_bytes();
        let start = *pos;
        if !bytes
            .get(*pos)
            .is_some_and(|b| b.is_ascii_alphabetic() || *b == b'_')
        {
            return None;
        }
        *pos += 1;
        while bytes
            .get(*pos)
            .is_some_and(|b| b.is_ascii_alphanumeric() || *b == b'_')
        {
            *pos += 1;
        }
        Some(&text[start..*pos])
    }
    fn parse(text: &str, pos: &mut usize, depth: usize, count: &mut usize) -> Option<JsonType> {
        if depth > 8 {
            return None;
        }
        let bytes = text.as_bytes();
        whitespace(bytes, pos);
        if bytes.get(*pos) == Some(&b'{') {
            *pos += 1;
            let mut fields = Vec::new();
            let mut seen = HashSet::new();
            loop {
                whitespace(bytes, pos);
                if bytes.get(*pos) == Some(&b'}') {
                    break;
                }
                let name = ident(text, pos)?.to_string();
                whitespace(bytes, pos);
                if bytes.get(*pos) != Some(&b':') || !seen.insert(name.clone()) {
                    return None;
                }
                *pos += 1;
                let field_type = parse(text, pos, depth + 1, count)?;
                *count += 1;
                if *count > 255 {
                    return None;
                }
                fields.push((name, field_type));
                whitespace(bytes, pos);
                if bytes.get(*pos) != Some(&b',') {
                    break;
                }
                *pos += 1;
                whitespace(bytes, pos);
                if bytes.get(*pos) == Some(&b'}') {
                    return None;
                }
            }
            if fields.is_empty() || bytes.get(*pos) != Some(&b'}') {
                return None;
            }
            *pos += 1;
            Some(JsonType::Record(fields))
        } else {
            let name = ident(text, pos)?;
            matches!(name, "String" | "Int" | "Float" | "Bool")
                .then(|| JsonType::Scalar(name.to_string()))
        }
    }
    let mut pos = 0;
    let mut count = 0;
    let schema = parse(ty, &mut pos, 0, &mut count)?;
    whitespace(ty.as_bytes(), &mut pos);
    (pos == ty.len()).then_some(schema)
}

/// Keep the flat record helper stable for existing scalar request bindings.
pub fn parse_flat_record_type(ty: &str) -> Option<Vec<(String, String)>> {
    let JsonType::Record(fields) = parse_json_type(ty)? else {
        return None;
    };
    fields
        .into_iter()
        .map(|(name, field_ty)| {
            if let JsonType::Scalar(ty) = field_ty {
                Some((name, ty))
            } else {
                None
            }
        })
        .collect()
}

#[derive(Debug, Clone, PartialEq)]
pub enum JsonBoundValue {
    Scalar(Constant),
    Record(Vec<(String, JsonBoundValue)>),
}

// Deserialize keys without losing duplicate-key evidence. serde_json::Value
// alone silently overwrites earlier keys, which is unsafe for API DTOs.
struct StrictObject(HashMap<String, Value>);

impl<'de> Deserialize<'de> for StrictObject {
    fn deserialize<D: Deserializer<'de>>(deserializer: D) -> Result<Self, D::Error> {
        struct ObjectVisitor;
        impl<'de> Visitor<'de> for ObjectVisitor {
            type Value = StrictObject;
            fn expecting(&self, f: &mut fmt::Formatter) -> fmt::Result {
                f.write_str("a JSON object with unique keys")
            }
            fn visit_map<M: MapAccess<'de>>(self, mut access: M) -> Result<Self::Value, M::Error> {
                let mut fields = HashMap::new();
                while let Some((name, value)) = access.next_entry::<String, Value>()? {
                    if fields.insert(name.clone(), value).is_some() {
                        return Err(serde::de::Error::custom(format!(
                            "duplicate JSON field '{name}'"
                        )));
                    }
                }
                Ok(StrictObject(fields))
            }
        }
        deserializer.deserialize_map(ObjectVisitor)
    }
}

/// Strict primitive-only JSON record decoder. Unknown keys are rejected,
/// preventing mass-assignment of fields absent from the declared input DTO.
pub fn decode_flat_record(
    raw: &str,
    fields: &[(String, String)],
) -> Result<Vec<(String, Constant)>, String> {
    let StrictObject(object) =
        serde_json::from_str(raw).map_err(|error| format!("invalid JSON object: {error}"))?;
    if object.len() != fields.len() {
        return Err("JSON object fields do not match declared request type".to_string());
    }
    let mut out = Vec::with_capacity(fields.len());
    for (name, ty) in fields {
        let item = object
            .get(name)
            .ok_or_else(|| format!("missing JSON field '{name}'"))?;
        let json_scalar =
            serde_json::to_string(item).map_err(|_| "invalid JSON field".to_string())?;
        let value =
            crate::web::request_bindings::decode_json_scalar_constant(&json_scalar, Some(ty))
                .map_err(|_| format!("invalid JSON field '{name}': expected {ty}"))?;
        out.push((name.clone(), value));
    }
    Ok(out)
}

/// Duplicate-preserving JSON tree; serde_json::Value alone discards duplicate
/// keys in nested maps before schema validation has an opportunity to reject.
#[derive(Debug)]
enum StrictJsonValue {
    Null,
    Bool(bool),
    Number(serde_json::Number),
    String(String),
    Array,
    Object(HashMap<String, StrictJsonValue>),
}

impl<'de> Deserialize<'de> for StrictJsonValue {
    fn deserialize<D: Deserializer<'de>>(deserializer: D) -> Result<Self, D::Error> {
        struct JsonVisitor;
        impl<'de> Visitor<'de> for JsonVisitor {
            type Value = StrictJsonValue;
            fn expecting(&self, f: &mut fmt::Formatter) -> fmt::Result {
                f.write_str("a JSON value")
            }
            fn visit_bool<E: serde::de::Error>(self, value: bool) -> Result<Self::Value, E> {
                Ok(StrictJsonValue::Bool(value))
            }
            fn visit_i64<E: serde::de::Error>(self, value: i64) -> Result<Self::Value, E> {
                Ok(StrictJsonValue::Number(value.into()))
            }
            fn visit_u64<E: serde::de::Error>(self, value: u64) -> Result<Self::Value, E> {
                Ok(StrictJsonValue::Number(value.into()))
            }
            fn visit_f64<E: serde::de::Error>(self, value: f64) -> Result<Self::Value, E> {
                serde_json::Number::from_f64(value)
                    .map(StrictJsonValue::Number)
                    .ok_or_else(|| E::custom("non-finite JSON number"))
            }
            fn visit_str<E: serde::de::Error>(self, value: &str) -> Result<Self::Value, E> {
                Ok(StrictJsonValue::String(value.to_string()))
            }
            fn visit_string<E: serde::de::Error>(self, value: String) -> Result<Self::Value, E> {
                Ok(StrictJsonValue::String(value))
            }
            fn visit_unit<E: serde::de::Error>(self) -> Result<Self::Value, E> {
                Ok(StrictJsonValue::Null)
            }
            fn visit_none<E: serde::de::Error>(self) -> Result<Self::Value, E> {
                Ok(StrictJsonValue::Null)
            }
            fn visit_seq<M: serde::de::SeqAccess<'de>>(
                self,
                mut access: M,
            ) -> Result<Self::Value, M::Error> {
                while access.next_element::<serde::de::IgnoredAny>()?.is_some() {}
                Ok(StrictJsonValue::Array)
            }
            fn visit_map<M: MapAccess<'de>>(self, mut access: M) -> Result<Self::Value, M::Error> {
                let mut fields = HashMap::new();
                while let Some((name, value)) = access.next_entry::<String, StrictJsonValue>()? {
                    if fields.insert(name.clone(), value).is_some() {
                        return Err(serde::de::Error::custom(format!(
                            "duplicate JSON field '{name}'"
                        )));
                    }
                }
                Ok(StrictJsonValue::Object(fields))
            }
        }
        deserializer.deserialize_any(JsonVisitor)
    }
}

/// Decode an exact recursively nested JSON object with no implicit coercions
/// or omitted/null values. This produces a VM-independent argument tree.
pub fn decode_nested_record(
    raw: &str,
    schema: &JsonType,
) -> Result<Vec<(String, JsonBoundValue)>, String> {
    let json: StrictJsonValue =
        serde_json::from_str(raw).map_err(|error| format!("invalid JSON body: {error}"))?;
    fn decode(value: StrictJsonValue, schema: &JsonType) -> Result<JsonBoundValue, String> {
        match (value, schema) {
            (StrictJsonValue::Object(mut fields), JsonType::Record(expected)) => {
                if fields.len() != expected.len() {
                    return Err("JSON object fields do not match declared request type".into());
                }
                let mut result = Vec::with_capacity(expected.len());
                for (name, ty) in expected {
                    let value = fields
                        .remove(name)
                        .ok_or_else(|| format!("missing JSON field '{name}'"))?;
                    result.push((
                        name.clone(),
                        decode(value, ty).map_err(|why| format!("JSON field '{name}': {why}"))?,
                    ));
                }
                Ok(JsonBoundValue::Record(result))
            }
            (StrictJsonValue::String(s), JsonType::Scalar(ty)) if ty == "String" => {
                Ok(JsonBoundValue::Scalar(Constant::String(s)))
            }
            (StrictJsonValue::Bool(b), JsonType::Scalar(ty)) if ty == "Bool" => {
                Ok(JsonBoundValue::Scalar(Constant::Bool(b)))
            }
            (StrictJsonValue::Number(n), JsonType::Scalar(ty)) if ty == "Int" => n
                .as_i64()
                .map(|n| JsonBoundValue::Scalar(Constant::Int(n)))
                .ok_or_else(|| "expected signed 64-bit JSON integer".into()),
            (StrictJsonValue::Number(n), JsonType::Scalar(ty)) if ty == "Float" => n
                .as_f64()
                .filter(|n| n.is_finite())
                .map(|n| JsonBoundValue::Scalar(Constant::Float(n)))
                .ok_or_else(|| "expected finite JSON float".into()),
            (_, _) => Err("JSON type does not match declared field type".into()),
        }
    }
    match decode(json, schema)? {
        JsonBoundValue::Record(fields) => Ok(fields),
        JsonBoundValue::Scalar(_) => Err("expected JSON object".into()),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn recursive_schema_accepts_closed_nested_records_and_rejects_unsupported_types() {
        let shape =
            parse_json_type("{ name: String, address: { city: String, zip: Int }, active: Bool }")
                .expect("nested JSON schema");
        assert_eq!(
            shape.wire_name(),
            "{ name: String, address: { city: String, zip: Int }, active: Bool }"
        );
        for bad in [
            "{ name: Option[String] }",
            "{ name: String, address: { city: String, city: String } }",
            "{ items: [Int] }",
            "{ nested: { bad: Unknown } }",
            "{ a: { b: Int } } trailing",
            "{ a: { b: Int, } }",
        ] {
            assert!(parse_json_type(bad).is_none(), "{bad}");
        }
        let mut nested = "Int".to_string();
        for _ in 0..10 {
            nested = format!("{{ child: {nested} }}");
        }
        assert!(parse_json_type(&nested).is_none(), "depth overflow");
    }

    #[test]
    fn nested_decode_detects_duplicate_keys_and_strict_field_shape() {
        let shape =
            parse_json_type("{ name: String, address: { city: String, zip: Int } }").unwrap();
        let valid = decode_nested_record(
            r#"{"name":"Ada","address":{"city":"London","zip":12345}}"#,
            &shape,
        )
        .unwrap();
        assert_eq!(valid.len(), 2);
        assert!(matches!(valid[1].1, JsonBoundValue::Record(_)));
        for bad in [
            r#"{"name":"Ada","address":{"city":"London","zip":1,"zip":2}}"#,
            r#"{"name":"Ada","address":{"city":"London","zip":1,"admin":true}}"#,
            r#"{"name":"Ada","address":{"city":"London"}}"#,
            r#"{"name":"Ada","address":{"city":null,"zip":1}}"#,
            r#"{"name":"Ada","address":[]}"#,
        ] {
            assert!(decode_nested_record(bad, &shape).is_err(), "{bad}");
        }
    }

    #[test]
    fn record_shape_rejects_nonprimitive_and_duplicate_fields() {
        assert!(parse_flat_record_type("{ name: String, age: Int }").is_some());
        for value in [
            "{ name: Address }",
            "{ xs: [Int] }",
            "{ name: String, name: String }",
            "{ empty: }",
            "{: String }",
        ] {
            assert!(parse_flat_record_type(value).is_none(), "{value}");
        }
    }

    #[test]
    fn decode_is_exact_and_rejects_extra_or_missing_fields() {
        let fields = parse_flat_record_type("{ name: String, age: Int }").unwrap();
        assert_eq!(
            decode_flat_record(r#"{"name":"Ada","age":42}"#, &fields).unwrap(),
            vec![
                ("name".to_string(), Constant::String("Ada".to_string())),
                ("age".to_string(), Constant::Int(42))
            ]
        );
        for payload in [
            r#"{"name":"Ada"}"#,
            r#"{"name":"Ada","age":"42"}"#,
            r#"{"name":"Ada","age":42,"role":"admin"}"#,
            r#"[]"#,
            r#"{"name":"Ada","age":42,"age":30}"#,
        ] {
            assert!(decode_flat_record(payload, &fields).is_err(), "{payload}");
        }
    }
}
