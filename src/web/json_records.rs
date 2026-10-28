//! Flat, compiler-described JSON record boundary.
//! Nested records, arrays, nullable values and coercion are intentionally
//! unsupported until the web ABI and schema compiler represent them precisely.

use crate::bytecode::Constant;
use serde::de::{MapAccess, Visitor};
use serde::{Deserialize, Deserializer};
use serde_json::Value;
use std::collections::{HashMap, HashSet};
use std::fmt;

/// Accept the compiler's closed record spelling: `{ name: String, age: Int }`.
/// Returning None for unsupported shapes ensures the compiler fails closed.
pub fn parse_flat_record_type(ty: &str) -> Option<Vec<(String, String)>> {
    let interior = ty.trim().strip_prefix('{')?.strip_suffix('}')?.trim();
    if interior.is_empty() {
        return None;
    }
    let mut seen = HashSet::new();
    let mut fields = Vec::new();
    for item in interior.split(',') {
        let (name, ty) = item.split_once(':')?;
        let (name, ty) = (name.trim(), ty.trim());
        if name.is_empty()
            || !name.chars().enumerate().all(|(i, c)| {
                c == '_'
                    || if i == 0 {
                        c.is_ascii_alphabetic()
                    } else {
                        c.is_ascii_alphanumeric()
                    }
            })
            || !matches!(ty, "String" | "Int" | "Float" | "Bool")
            || !seen.insert(name.to_string())
        {
            return None;
        }
        fields.push((name.to_string(), ty.to_string()));
    }
    (fields.len() <= 255).then_some(fields)
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

#[cfg(test)]
mod tests {
    use super::*;

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
