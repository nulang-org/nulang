//! Experimental, transport-neutral typed form contracts derived from Web route bindings.
//!
//! Form metadata reuses the compiler-owned route binding plan. Additional
//! presentation/validation rules are explicit Rust-side metadata until Nulang
//! grows a stable source-level form syntax. No database or browser runtime is
//! needed to validate or render a form.

use crate::bytecode::Constant;
use crate::web::bindings::{compile_route_bindings, RouteBindingSource};
use crate::web::contracts::RouteContract;
use crate::web::request_bindings::decode_scalar_constant;
use serde_json::{json, Value};
use std::collections::{BTreeMap, HashMap, HashSet};

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum FormFieldType {
    Text,
    Int,
    Float,
    Bool,
}

impl FormFieldType {
    fn from_source_type(source: Option<&str>) -> Self {
        match source.map(str::trim) {
            Some("Int") => Self::Int,
            Some("Float") => Self::Float,
            Some("Bool") => Self::Bool,
            // Domain/opaque types remain string-backed in current Web decoding.
            _ => Self::Text,
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq)]
pub enum FormRule {
    MinLength(usize),
    MaxLength(usize),
    MinInt(i64),
    MaxInt(i64),
    MinFloat(f64),
    MaxFloat(f64),
}

#[derive(Debug, Clone, PartialEq)]
pub struct FormFieldContract {
    pub name: String,
    pub handler_param: String,
    pub source_type: Option<String>,
    pub field_type: FormFieldType,
    pub rules: Vec<FormRule>,
}

#[derive(Debug, Clone, PartialEq)]
pub struct FormContract {
    pub method: String,
    pub path: String,
    pub fields: Vec<FormFieldContract>,
}

#[derive(Debug, Clone, PartialEq)]
pub struct ValidatedForm {
    pub values: BTreeMap<String, Constant>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct FormFieldError {
    pub field: String,
    pub code: &'static str,
    pub message: &'static str,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct FormValidationErrors {
    pub issues: Vec<FormFieldError>,
}

impl FormValidationErrors {
    /// An HTTP adapter may expose this as a 422 application/problem+json body.
    /// No submitted values are echoed into the diagnostic payload.
    pub fn problem_details(&self) -> Value {
        let errors: Vec<_> = self
            .issues
            .iter()
            .map(|issue| {
                json!({
                    "field": issue.field,
                    "code": issue.code,
                    "message": issue.message,
                })
            })
            .collect();
        json!({
            "type": "urn:nulang:web:form-validation",
            "title": "Invalid form input",
            "status": 422,
            "errors": errors,
        })
    }
}

impl FormContract {
    /// Project the exact compiler binding plan into a form contract.
    ///
    /// Mixed-source handlers are supported; query, path and body bindings
    /// remain the responsibility of the existing request binder.
    pub fn from_route(route: &RouteContract) -> Result<Self, Vec<String>> {
        let compilation = compile_route_bindings(route);
        if !compilation.diagnostics.is_empty() {
            return Err(compilation.diagnostics);
        }

        let mut seen = HashSet::new();
        let mut fields = Vec::new();
        let mut diagnostics = Vec::new();

        for binding in compilation
            .bindings
            .iter()
            .filter(|binding| binding.source == RouteBindingSource::Form)
        {
            if !seen.insert(binding.source_name.clone()) {
                diagnostics.push(format!(
                    "{} {}: duplicate form field '{}'",
                    route.method, route.path, binding.source_name
                ));
                continue;
            }
            fields.push(FormFieldContract {
                name: binding.source_name.clone(),
                handler_param: binding.handler_param.clone(),
                source_type: binding.ty.clone(),
                field_type: FormFieldType::from_source_type(binding.ty.as_deref()),
                rules: Vec::new(),
            });
        }

        if !diagnostics.is_empty() {
            return Err(diagnostics);
        }

        Ok(Self {
            method: route.method.clone(),
            path: route.path.clone(),
            fields,
        })
    }

    /// Attach validated rules without changing Nulang's frozen grammar or
    /// claiming that the compiler already knows source-level constraints.
    pub fn with_rules(mut self, name: &str, rules: Vec<FormRule>) -> Result<Self, String> {
        let Some(field) = self.fields.iter_mut().find(|field| field.name == name) else {
            return Err(format!("unknown form field '{name}'"));
        };
        check_rules(field.field_type, &rules)?;
        field.rules = rules;
        Ok(self)
    }

    /// Validate all compiler-declared form fields in deterministic handler order.
    /// Extra form keys are ignored (e.g. CSRF tokens or application metadata).
    /// All declared fields are required until optional typed bindings exist.
    pub fn validate(
        &self,
        submitted: &HashMap<String, Vec<String>>,
    ) -> Result<ValidatedForm, FormValidationErrors> {
        let mut values = BTreeMap::new();
        let mut issues = Vec::new();

        for field in &self.fields {
            let raw = match submitted.get(&field.name).map(Vec::as_slice) {
                None | Some([]) => {
                    issues.push(issue(field, "required", "This field is required"));
                    continue;
                }
                Some([raw]) => raw.as_str(),
                Some(_) => {
                    issues.push(issue(field, "multiple", "Expected a single field value"));
                    continue;
                }
            };

            // HTML required controls disallow empty strings; enforce the same
            // property server-side instead of trusting browser validation.
            if raw.is_empty() {
                issues.push(issue(field, "required", "This field is required"));
                continue;
            }

            let value = match decode_scalar_constant(raw, field.source_type.as_deref()) {
                Ok(value) => value,
                Err(_) => {
                    issues.push(issue(field, "invalid_type", "Invalid field value"));
                    continue;
                }
            };

            if let Some(rule_issue) = check_value(field, &value) {
                issues.push(rule_issue);
            } else {
                values.insert(field.name.clone(), value);
            }
        }

        if issues.is_empty() {
            Ok(ValidatedForm { values })
        } else {
            Err(FormValidationErrors { issues })
        }
    }

    /// Generate a small accessible, safely escaped HTML fragment. Applications
    /// own their form wrapper, action URL, CSRF token, and submission handling.
    /// Boolean fields use a select so both true and false can be submitted.
    pub fn render_fields(
        &self,
        submitted: &HashMap<String, Vec<String>>,
        errors: Option<&FormValidationErrors>,
    ) -> String {
        let mut html = String::new();
        for (index, field) in self.fields.iter().enumerate() {
            let id = format!("nulang-field-{index}");
            let name = escape_html(&field.name);
            let raw = submitted
                .get(&field.name)
                .and_then(|values| (values.len() == 1).then(|| values[0].as_str()))
                .unwrap_or("");
            let value = escape_html(raw);
            let error =
                errors.and_then(|errors| errors.issues.iter().find(|e| e.field == field.name));

            html.push_str(&format!("<label for=\"{id}\">{name}</label>"));
            let mut attributes = format!("id=\"{id}\" name=\"{name}\" required");
            if error.is_some() {
                attributes.push_str(&format!(
                    " aria-invalid=\"true\" aria-describedby=\"{id}-error\""
                ));
            }

            match field.field_type {
                FormFieldType::Bool => {
                    html.push_str(&format!("<select {attributes}>"));
                    html.push_str("<option value=\"\">Select an option</option>");
                    for choice in ["true", "false"] {
                        let selected = if raw == choice { " selected" } else { "" };
                        html.push_str(&format!(
                            "<option value=\"{choice}\"{selected}>{choice}</option>"
                        ));
                    }
                    html.push_str("</select>");
                }
                kind => {
                    let (input_type, step) = match kind {
                        FormFieldType::Text => ("text", ""),
                        FormFieldType::Int => ("number", " step=\"1\""),
                        FormFieldType::Float => ("number", " step=\"any\""),
                        FormFieldType::Bool => unreachable!(),
                    };
                    for rule in &field.rules {
                        let attr = match rule {
                            FormRule::MinLength(value) => format!(" minlength=\"{value}\""),
                            FormRule::MaxLength(value) => format!(" maxlength=\"{value}\""),
                            FormRule::MinInt(value) => format!(" min=\"{value}\""),
                            FormRule::MaxInt(value) => format!(" max=\"{value}\""),
                            FormRule::MinFloat(value) => format!(" min=\"{value}\""),
                            FormRule::MaxFloat(value) => format!(" max=\"{value}\""),
                        };
                        attributes.push_str(&attr);
                    }
                    html.push_str(&format!(
                        "<input {attributes} type=\"{input_type}\"{step} value=\"{value}\">"
                    ));
                }
            }

            if let Some(error) = error {
                html.push_str(&format!(
                    "<span id=\"{id}-error\" role=\"alert\">{}</span>",
                    escape_html(error.message)
                ));
            }
        }
        html
    }
}

fn issue(field: &FormFieldContract, code: &'static str, message: &'static str) -> FormFieldError {
    FormFieldError {
        field: field.name.clone(),
        code,
        message,
    }
}

fn check_rules(field_type: FormFieldType, rules: &[FormRule]) -> Result<(), String> {
    let mut seen = HashSet::new();
    let mut min_length = None;
    let mut max_length = None;
    let mut min_int = None;
    let mut max_int = None;
    let mut min_float = None;
    let mut max_float = None;

    for rule in rules {
        if !seen.insert(std::mem::discriminant(rule)) {
            return Err("a form rule type may only appear once per field".into());
        }
        match (field_type, rule) {
            (FormFieldType::Text, FormRule::MinLength(value)) => min_length = Some(*value),
            (FormFieldType::Text, FormRule::MaxLength(value)) => max_length = Some(*value),
            (FormFieldType::Int, FormRule::MinInt(value)) => min_int = Some(*value),
            (FormFieldType::Int, FormRule::MaxInt(value)) => max_int = Some(*value),
            (FormFieldType::Float, FormRule::MinFloat(value)) if value.is_finite() => {
                min_float = Some(*value)
            }
            (FormFieldType::Float, FormRule::MaxFloat(value)) if value.is_finite() => {
                max_float = Some(*value)
            }
            _ => return Err(format!("rule {rule:?} is incompatible with {field_type:?}")),
        }
    }
    if min_length
        .zip(max_length)
        .is_some_and(|(min, max)| min > max)
        || min_int.zip(max_int).is_some_and(|(min, max)| min > max)
        || min_float.zip(max_float).is_some_and(|(min, max)| min > max)
    {
        return Err("minimum form constraint exceeds maximum".into());
    }
    Ok(())
}

fn check_value(field: &FormFieldContract, value: &Constant) -> Option<FormFieldError> {
    for rule in &field.rules {
        let violation = match (rule, value) {
            (FormRule::MinLength(min), Constant::String(value)) if value.chars().count() < *min => {
                Some(("too_short", "Value is too short"))
            }
            (FormRule::MaxLength(max), Constant::String(value)) if value.chars().count() > *max => {
                Some(("too_long", "Value is too long"))
            }
            (FormRule::MinInt(min), Constant::Int(value)) if value < min => {
                Some(("too_small", "Value is too small"))
            }
            (FormRule::MaxInt(max), Constant::Int(value)) if value > max => {
                Some(("too_large", "Value is too large"))
            }
            (FormRule::MinFloat(min), Constant::Float(value)) if value < min => {
                Some(("too_small", "Value is too small"))
            }
            (FormRule::MaxFloat(max), Constant::Float(value)) if value > max => {
                Some(("too_large", "Value is too large"))
            }
            _ => None,
        };
        if let Some((code, message)) = violation {
            return Some(issue(field, code, message));
        }
    }
    None
}

fn escape_html(input: &str) -> String {
    input
        .replace('&', "&amp;")
        .replace('<', "&lt;")
        .replace('>', "&gt;")
        .replace('"', "&quot;")
        .replace('\'', "&#x27;")
}
