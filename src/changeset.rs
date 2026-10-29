//! Explicit input casting and structured validation inspired by Ecto changesets.
//! This pure Rust support API is not yet a source-level Nulang language feature.

use serde_json::{Map, Number, Value};

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum FieldType {
    String,
    Integer,
    Float,
    Boolean,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct FieldSpec<'a> {
    pub name: &'a str,
    pub ty: FieldType,
    pub required: bool,
}

impl<'a> FieldSpec<'a> {
    pub fn required(name: &'a str, ty: FieldType) -> Self {
        Self {
            name,
            ty,
            required: true,
        }
    }

    pub fn optional(name: &'a str, ty: FieldType) -> Self {
        Self {
            name,
            ty,
            required: false,
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct FieldError {
    pub field: String,
    pub code: &'static str,
}

#[derive(Debug, Clone, Default)]
pub struct Changeset {
    fields: Map<String, Value>,
    errors: Vec<FieldError>,
}

impl Changeset {
    /// Unknown input keys are never copied. Failed casts are recorded as errors
    /// and excluded from the output rather than silently preserving raw values.
    pub fn cast(raw: &Value, fields: &[FieldSpec<'_>]) -> Self {
        let Some(input) = raw.as_object() else {
            return Self {
                fields: Map::new(),
                errors: vec![FieldError {
                    field: "$".into(),
                    code: "object_required",
                }],
            };
        };
        let mut result = Self::default();
        for spec in fields {
            match input.get(spec.name) {
                None | Some(Value::Null) => {
                    if spec.required {
                        result.errors.push(FieldError {
                            field: spec.name.into(),
                            code: "required",
                        });
                    }
                }
                Some(value) => {
                    if let Some(parsed) = cast_value(value, spec.ty) {
                        if spec.required && parsed.as_str().is_some_and(|s| s.trim().is_empty()) {
                            result.errors.push(FieldError {
                                field: spec.name.into(),
                                code: "required",
                            });
                        } else {
                            result.fields.insert(spec.name.into(), parsed);
                        }
                    } else {
                        result.errors.push(FieldError {
                            field: spec.name.into(),
                            code: "cast",
                        });
                    }
                }
            }
        }
        result
    }

    /// Validate the minimum value of an already-cast numeric field.
    pub fn validate_min(mut self, name: &str, min: i64) -> Self {
        if let Some(value) = self.fields.get(name) {
            let valid = value
                .as_i64()
                .map(|n| n >= min)
                .or_else(|| value.as_f64().map(|n| n >= min as f64))
                .unwrap_or(false);
            if !valid {
                self.errors.push(FieldError {
                    field: name.into(),
                    code: "min",
                });
            }
        }
        self
    }

    pub fn is_valid(&self) -> bool {
        self.errors.is_empty()
    }

    pub fn errors(&self) -> &[FieldError] {
        &self.errors
    }

    /// Return allowed and cast values, or all accumulated validation errors.
    pub fn into_result(self) -> Result<Map<String, Value>, Vec<FieldError>> {
        if self.errors.is_empty() {
            Ok(self.fields)
        } else {
            Err(self.errors)
        }
    }
}

fn cast_value(value: &Value, ty: FieldType) -> Option<Value> {
    match ty {
        FieldType::String => value.as_str().map(|s| Value::String(s.to_owned())),
        FieldType::Integer => match value {
            Value::Number(n) if n.as_i64().is_some() => Some(value.clone()),
            Value::String(s) => s.parse::<i64>().ok().map(|n| Value::Number(n.into())),
            _ => None,
        },
        FieldType::Float => match value {
            Value::Number(_) => value.as_f64().and_then(Number::from_f64).map(Value::Number),
            Value::String(s) => s
                .parse::<f64>()
                .ok()
                .and_then(Number::from_f64)
                .map(Value::Number),
            _ => None,
        },
        FieldType::Boolean => match value {
            Value::Bool(_) => Some(value.clone()),
            Value::String(s) if s == "true" || s == "1" => Some(Value::Bool(true)),
            Value::String(s) if s == "false" || s == "0" => Some(Value::Bool(false)),
            _ => None,
        },
    }
}
