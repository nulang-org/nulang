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
    /// "error" | "warning" | "note"
    pub severity: String,
    pub message: String,
    pub span: Option<JsonSpan>,
    pub notes: Vec<String>,
    pub suggestion: Option<JsonSuggestion>,
}

/// 1-indexed source span.
#[derive(Debug, Clone, Serialize)]
pub struct JsonSpan {
    pub file: String,
    pub line: usize,
    pub col: usize,
    pub end_line: usize,
    pub end_col: usize,
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
    JsonDiagnostic {
        code: err.stable_code().map(|s| s.to_string()),
        severity: "error".to_string(),
        message: json_message(err),
        span: err.primary_span().and_then(json_span),
        notes: crate::diagnostic::diagnostic_notes(err),
        suggestion: err.suggestion().map(|msg| JsonSuggestion {
            message: msg.to_string(),
            replacement: None,
        }),
    }
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
        severity: "error".to_string(),
        message,
        span: None,
        notes: Vec::new(),
        suggestion: None,
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
        let span = d.span.as_ref().expect("span");
        assert_eq!(span.line, 1);
        assert_eq!(span.col, 13);
        assert_eq!(span.end_col, 19);
        assert_eq!(span.file, "test.nula");
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
