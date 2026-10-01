//! Machine-readable JSON diagnostics (`--json`) for the Nulang CLI.
//!
//! This module is a **serialization view** over the existing diagnostic
//! pipeline: it reuses [`NuError`], [`NuError::stable_code`], the structured
//! notes from [`crate::diagnostic`], and the thread-local
//! [`SourceMap`](crate::types::SourceMap) installed by the lexer to resolve
//! byte-offset spans to 1-indexed line/column positions. It does not change
//! how diagnostics are produced or how the human renderer prints them.
//!
//! Schema (top-level object, emitted as the ONLY bytes on stdout when
//! `--json` is passed; progress/logging stays on stderr):
//!
//! ```json
//! {
//!   "schema_version": 1,
//!   "command": "check",
//!   "file": "path/to/source.nula",
//!   "ok": false,
//!   "diagnostics": [
//!     {
//!       "code": "E0207",
//!       "severity": "error",
//!       "message": "...",
//!       "span": { "file": "...", "line": 1, "col": 5, "end_line": 1, "end_col": 12 },
//!       "notes": ["..."],
//!       "suggestion": { "message": "...", "replacement": null }
//!     }
//!   ]
//! }
//! ```
//!
//! For `nula test --json`, an additional `"tests"` array carries per-test
//! results (`name`, `status`, `duration_ms`, and `diagnostics` on failure).

use serde::Serialize;

use crate::types::{current_source_text, source_map_file, NuError, Span};

/// Current JSON diagnostics schema version.
pub const SCHEMA_VERSION: u32 = 1;

/// Top-level report object emitted on stdout in `--json` mode.
#[derive(Debug, Clone, Serialize)]
pub struct JsonReport {
    pub schema_version: u32,
    pub command: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub file: Option<String>,
    pub ok: bool,
    pub diagnostics: Vec<JsonDiagnostic>,
    /// Per-test results; only present for the `test` command.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub tests: Option<Vec<JsonTestResult>>,
}

impl JsonReport {
    pub fn new(command: &str, file: Option<String>, diagnostics: Vec<JsonDiagnostic>) -> Self {
        let ok = diagnostics.iter().all(|d| d.severity != "error");
        JsonReport {
            schema_version: SCHEMA_VERSION,
            command: command.to_string(),
            file,
            ok,
            diagnostics,
            tests: None,
        }
    }

    /// Serialize as a single-line JSON object (one trailing newline).
    pub fn to_json_string(&self) -> String {
        let mut s = serde_json::to_string(self).unwrap_or_else(|_| {
            "{\"schema_version\":1,\"ok\":false,\"diagnostics\":[]}".to_string()
        });
        s.push('\n');
        s
    }
}

/// One machine-readable diagnostic.
#[derive(Debug, Clone, Serialize)]
pub struct JsonDiagnostic {
    /// Stable error code (`E0101`-style), or null when the error has none.
    pub code: Option<String>,
    /// Stable machine-oriented category such as `unbound_variable`.
    pub kind: String,
    /// "error" | "warning" | "note"
    pub severity: String,
    pub message: String,
    pub span: Option<JsonSpan>,
    /// Structured diagnostic payload. Consumers should prefer this over parsing
    /// `message` or `notes`.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub data: Option<serde_json::Value>,
    pub notes: Vec<String>,
    /// Legacy human-oriented suggestion. Kept for compatibility.
    pub suggestion: Option<JsonSuggestion>,
    /// Zero or more edits that tooling may offer or apply.
    pub fixes: Vec<JsonFix>,
}

/// 1-indexed source span plus exact UTF-8 byte offsets for edits.
#[derive(Debug, Clone, Serialize)]
pub struct JsonSpan {
    pub file: String,
    pub start_byte: u32,
    pub end_byte: u32,
    pub line: usize,
    pub col: usize,
    pub end_line: usize,
    pub end_col: usize,
}

#[derive(Debug, Clone, Serialize)]
pub struct JsonFix {
    pub message: String,
    /// `machine_applicable` means the edit is unambiguous and can be applied
    /// without interpreting diagnostic prose.
    pub applicability: String,
    pub edits: Vec<JsonEdit>,
}

#[derive(Debug, Clone, Serialize)]
pub struct JsonEdit {
    pub file: String,
    pub start_byte: u32,
    pub end_byte: u32,
    pub replacement: String,
}

/// A suggested fix. `message` mirrors the existing human-facing help text;
/// `replacement` stays null unless a machine-applicable edit exists.
#[derive(Debug, Clone, Serialize)]
pub struct JsonSuggestion {
    pub message: String,
    pub replacement: Option<String>,
}

/// Per-test result for `nula test --json`.
#[derive(Debug, Clone, Serialize)]
pub struct JsonTestResult {
    pub name: String,
    /// "ok" | "failed"
    pub status: String,
    pub duration_ms: u64,
    pub diagnostics: Vec<JsonDiagnostic>,
}

/// Convert an [`NuError`] into a flat list of JSON diagnostics.
///
/// [`NuError::Multiple`] is flattened (each child keeps its own code/span);
/// [`NuError::Suspended`] is not an error and yields nothing.
pub fn diagnostics_from_error(err: &NuError) -> Vec<JsonDiagnostic> {
    match err {
        NuError::Multiple(errors) => errors
            .iter()
            .flat_map(|e| diagnostics_from_error(e))
            .collect(),
        NuError::Suspended(_) => Vec::new(),
        _ => vec![diagnostic_from_single(err)],
    }
}

fn diagnostic_from_single(err: &NuError) -> JsonDiagnostic {
    let primary_span = err.primary_span();
    JsonDiagnostic {
        code: err.stable_code().map(|s| s.to_string()),
        kind: diagnostic_kind(err).to_string(),
        severity: "error".to_string(),
        message: json_message(err),
        span: primary_span.and_then(json_span),
        data: diagnostic_data(err),
        notes: crate::diagnostic::diagnostic_notes(err),
        suggestion: err.suggestion().map(|msg| JsonSuggestion {
            message: msg.to_string(),
            replacement: None,
        }),
        fixes: diagnostic_fixes(err, primary_span),
    }
}

fn diagnostic_kind(err: &NuError) -> &'static str {
    match err.stable_code() {
        Some("E0101") => "lex_error",
        Some("E0102") => "parse_error",
        Some("E0103") => "unclosed_delimiter",
        Some("E0201") => "type_mismatch",
        Some("E0202") => "unbound_variable",
        Some("E0203") => "infinite_type",
        Some("E0204") => "field_not_found",
        Some("E0205") => "wrong_arity",
        Some("E0206") => "empty_match",
        Some("E0208") => "ffi_boundary_violation",
        Some("E0301") => "missing_effect",
        Some("E0302") => "unhandled_effect",
        Some("E0401") => "sendability_violation",
        Some("E0402") => "linear_use_after_consume",
        Some("E0503") => "step_limit_exceeded",
        Some("E0601") => "ffi_error",
        Some("E0602") => "python_error",
        Some("E0901") => "not_yet_implemented",
        Some("E0902") => "package_error",
        Some("E0200") => "type_error",
        Some("E0300") => "effect_error",
        Some("E0400") => "capability_error",
        Some("E0501") => "runtime_error",
        Some("E0502") => "vm_error",
        _ => "unknown_error",
    }
}

fn diagnostic_data(err: &NuError) -> Option<serde_json::Value> {
    match err {
        NuError::ParseError {
            expected, found, ..
        } if expected.is_some() || found.is_some() => Some(serde_json::json!({
            "expected": expected,
            "found": found,
        })),
        NuError::TypeError {
            msg,
            expected_type,
            found_type,
            similar_names,
            ..
        } => {
            let mut data = serde_json::Map::new();
            if let Some(name) = unbound_name(msg) {
                data.insert("name".to_string(), serde_json::json!(name));
            }
            if let Some(expected) = expected_type {
                data.insert("expected_type".to_string(), serde_json::json!(expected));
            }
            if let Some(found) = found_type {
                data.insert("found_type".to_string(), serde_json::json!(found));
            }
            if let Some(names) = similar_names {
                data.insert("candidates".to_string(), serde_json::json!(names));
            }
            if data.is_empty() {
                None
            } else {
                Some(serde_json::Value::Object(data))
            }
        }
        NuError::EffectError {
            missing_effects,
            allowed_effects,
            ..
        } if missing_effects.is_some() || allowed_effects.is_some() => Some(serde_json::json!({
            "missing_effects": missing_effects,
            "allowed_effects": allowed_effects,
        })),
        NuError::CapError {
            explanation: Some(explanation),
            ..
        } => Some(serde_json::json!({ "explanation": explanation })),
        _ => None,
    }
}

fn diagnostic_fixes(err: &NuError, primary_span: Option<Span>) -> Vec<JsonFix> {
    let Some(span) = primary_span else {
        return Vec::new();
    };

    match err {
        NuError::TypeError {
            msg,
            similar_names: Some(names),
            ..
        } if unbound_name(msg).is_some() && names.len() == 1 => {
            let Some(unbound) = unbound_name(msg) else {
                return Vec::new();
            };
            let Some(source) = current_source_text() else {
                return Vec::new();
            };
            let start = span.start as usize;
            let end = span.end as usize;
            if start > end
                || end > source.len()
                || !source.is_char_boundary(start)
                || !source.is_char_boundary(end)
                || &source[start..end] != unbound
            {
                // Never mark an edit machine-applicable unless the currently
                // installed source map proves that the diagnostic span names
                // exactly the identifier we intend to replace. Import
                // resolution can temporarily install a dependency's source
                // map; a mismatch must degrade to a diagnostic-only result.
                return Vec::new();
            }

            let replacement = names[0].clone();
            let file = source_map_file().unwrap_or_else(|| "<input>".to_string());
            vec![JsonFix {
                message: format!("replace with `{replacement}`"),
                applicability: "machine_applicable".to_string(),
                edits: vec![JsonEdit {
                    file,
                    start_byte: span.start,
                    end_byte: span.end,
                    replacement,
                }],
            }]
        }
        _ => Vec::new(),
    }
}

fn unbound_name(msg: &str) -> Option<&str> {
    msg.strip_prefix("Unbound variable: '")?.strip_suffix("'")
}

/// The core message without position prefixes or structured-field suffixes.
fn json_message(err: &NuError) -> String {
    match err {
        NuError::LexError { msg, .. }
        | NuError::ParseError { msg, .. }
        | NuError::TypeError { msg, .. }
        | NuError::EffectError { msg, .. }
        | NuError::CapError { msg, .. }
        | NuError::FFIError { msg, .. }
        | NuError::RuntimeError { msg, .. }
        | NuError::VMError { msg, .. }
        | NuError::PythonError { msg, .. }
        | NuError::PackageError { msg, .. } => msg.clone(),
        NuError::NotYetImplemented { feature, .. } => feature.clone(),
        NuError::Suspended(kind) => format!("VM suspended: {kind}"),
        NuError::Multiple(_) => String::new(),
    }
}

/// Resolve a byte-offset [`Span`] to 1-indexed line/col using the
/// thread-local source text installed by the lexer. Returns `None` when no
/// source is available.
fn json_span(span: Span) -> Option<JsonSpan> {
    let source = current_source_text()?;
    let file = source_map_file().unwrap_or_else(|| "<input>".to_string());
    let len = source.len() as u32;
    let start = span.start.min(len);
    let end = span.end.min(len).max(start);
    let (line, col) = offset_line_col(&source, start);
    let (end_line, end_col) = offset_line_col(&source, end);
    Some(JsonSpan {
        file,
        start_byte: start,
        end_byte: end,
        line,
        col,
        end_line,
        end_col,
    })
}

/// Resolve a byte offset to (1-indexed line, 1-indexed column). Columns count
/// bytes, matching `SourceMap::line_col` (ASCII-fast; lines split on `\n`).
fn offset_line_col(source: &str, offset: u32) -> (usize, usize) {
    let mut line = 1usize;
    let mut col = 1usize;
    for (i, &b) in source.as_bytes().iter().enumerate() {
        if i as u32 >= offset {
            break;
        }
        if b == b'\n' {
            line += 1;
            col = 1;
        } else {
            col += 1;
        }
    }
    (line, col)
}

/// Build a single error diagnostic from a plain message (used by `nula test`
/// failures, where the failing test's captured stderr is the only detail).
pub fn diagnostic_from_message(message: String) -> JsonDiagnostic {
    JsonDiagnostic {
        code: None,
        kind: "test_failure".to_string(),
        severity: "error".to_string(),
        message,
        span: None,
        data: None,
        notes: Vec::new(),
        suggestion: None,
        fixes: Vec::new(),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::types::{clear_source_map, set_source_map_with_file};

    #[test]
    fn test_report_serializes_expected_shape() {
        set_source_map_with_file("fn main() = countr + 1\n", Some("test.nula"));
        let start = "fn main() = ".len() as u32;
        let err = NuError::unbound_variable(
            "countr",
            Span::new(start, start + 6),
            Some(vec!["counter".to_string()]),
        );
        let diags = diagnostics_from_error(&err);
        assert_eq!(diags.len(), 1);
        let d = &diags[0];
        assert_eq!(d.code.as_deref(), Some("E0202"));
        assert_eq!(d.severity, "error");
        assert_eq!(d.kind, "unbound_variable");
        let span = d.span.as_ref().expect("span");
        assert_eq!(span.line, 1);
        assert_eq!(span.col, 13);
        assert_eq!(span.end_col, 19);
        assert_eq!(span.file, "test.nula");
        assert_eq!(span.start_byte, start);
        assert_eq!(span.end_byte, start + 6);
        assert_eq!(d.data.as_ref().unwrap()["name"], "countr");
        assert_eq!(d.fixes.len(), 1);
        assert_eq!(d.fixes[0].applicability, "machine_applicable");
        assert_eq!(d.fixes[0].edits[0].replacement, "counter");
        assert!(d
            .notes
            .iter()
            .any(|n| n.contains("did you mean one of: counter?")));

        let report = JsonReport::new("check", Some("test.nula".to_string()), diags);
        let v: serde_json::Value =
            serde_json::from_str(&report.to_json_string()).expect("valid json");
        assert_eq!(v["schema_version"], 1);
        assert_eq!(v["command"], "check");
        assert_eq!(v["ok"], false);
        assert!(v["diagnostics"].is_array());
        clear_source_map();
    }

    #[test]
    fn test_machine_fix_requires_span_to_match_current_source() {
        set_source_map_with_file("fn main() = other + 1\n", Some("dependency.nula"));
        let start = "fn main() = ".len() as u32;
        let err = NuError::unbound_variable(
            "countr",
            Span::new(start, start + 6),
            Some(vec!["counter".to_string()]),
        );

        let diags = diagnostics_from_error(&err);
        assert_eq!(diags.len(), 1);
        assert!(
            diags[0].fixes.is_empty(),
            "a span that does not name the diagnosed identifier must never become machine-applicable"
        );
        clear_source_map();
    }

    #[test]
    fn test_multiple_flattens() {
        let span = Span::default();
        let errs = NuError::Multiple(vec![
            NuError::LexError {
                msg: "bad char".into(),
                span,
            },
            NuError::parse_error("oops".into(), span),
        ]);
        let diags = diagnostics_from_error(&errs);
        assert_eq!(diags.len(), 2);
        assert_eq!(diags[0].code.as_deref(), Some("E0101"));
        assert_eq!(diags[1].code.as_deref(), Some("E0102"));
    }

    #[test]
    fn test_ok_report_has_empty_diagnostics() {
        let report = JsonReport::new("check", Some("ok.nula".to_string()), Vec::new());
        assert!(report.ok);
        let v: serde_json::Value =
            serde_json::from_str(&report.to_json_string()).expect("valid json");
        assert_eq!(v["ok"], true);
        assert_eq!(v["diagnostics"].as_array().unwrap().len(), 0);
    }
}
