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
    /// A replacement for a verified source slice; null for advisory help.
    pub replacement: Option<String>,
    /// Source-anchored text edit. Additive optional field in JSON schema v1.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub edit: Option<JsonTextEdit>,
    /// Suggestions require semantic review and rechecking after application.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub applicability: Option<String>,
}

/// Byte offsets (UTF-8), not character counts or LSP UTF-16 columns.
/// Consumers must verify `expected_text` before applying the replacement.
#[derive(Debug, Clone, Serialize)]
pub struct JsonTextEdit {
    pub file: String,
    pub start_byte: u32,
    pub end_byte: u32,
    pub expected_text: String,
    pub new_text: String,
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
        suggestion: unbound_variable_text_edit(err).or_else(|| {
            err.suggestion().map(|msg| JsonSuggestion {
                message: msg.to_string(),
                replacement: None,
                edit: None,
                applicability: None,
            })
        }),
    }
}

/// Only propose a concrete edit for a uniquely suggested in-scope name whose
/// source span exactly matches the unknown identifier. A close name is not
/// proof of programmer intent, so these edits always require review and a
/// subsequent compiler check. Never infer an edit from free-form help text.
fn unbound_variable_text_edit(err: &NuError) -> Option<JsonSuggestion> {
    let NuError::TypeError {
        msg,
        span,
        similar_names: Some(names),
        ..
    } = err
    else {
        return None;
    };
    if err.stable_code() != Some("E0202") {
        return None;
    }
    let [candidate] = names.as_slice() else {
        return None;
    };
    let original = msg
        .strip_prefix("Unbound variable: '")?
        .strip_suffix('\'')?;
    if original.is_empty() || candidate.is_empty() || original == candidate {
        return None;
    }

    let source = current_source_text()?;
    let start = span.start as usize;
    let end = span.end as usize;
    if start >= end || source.get(start..end)? != original {
        return None;
    }
    let file = source_map_file()?;
    // A synthetic / REPL filename is not a safely targetable source file.
    if file.is_empty() || (file.starts_with('<') && file.ends_with('>')) {
        return None;
    }

    Some(JsonSuggestion {
        message: format!("consider replacing `{original}` with `{candidate}`"),
        replacement: Some(candidate.clone()),
        edit: Some(JsonTextEdit {
            file,
            start_byte: span.start,
            end_byte: span.end,
            expected_text: original.to_string(),
            new_text: candidate.clone(),
        }),
        applicability: Some("requires-review".to_string()),
    })
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
    fn test_unbound_variable_exposes_reviewable_text_edit() {
        let source = "let counter = 1\nlet value = countr + 1\n";
        set_source_map_with_file(source, Some("program.nula"));
        let start = source.find("countr +").unwrap() as u32;
        let error = NuError::unbound_variable(
            "countr",
            Span::new(start, start + "countr".len() as u32),
            Some(vec!["counter".into()]),
        );

        let diagnostics = diagnostics_from_error(&error);
        let suggestion = diagnostics[0].suggestion.as_ref().expect("suggestion");
        assert_eq!(suggestion.replacement.as_deref(), Some("counter"));
        assert_eq!(suggestion.applicability.as_deref(), Some("requires-review"));
        let edit = suggestion.edit.as_ref().expect("machine-readable edit");
        assert_eq!(edit.file, "program.nula");
        assert_eq!(edit.start_byte, start);
        assert_eq!(edit.end_byte, start + 6);
        assert_eq!(edit.expected_text, "countr");
        assert_eq!(edit.new_text, "counter");

        let report = JsonReport::new("check", Some("program.nula".into()), diagnostics);
        let json: serde_json::Value = serde_json::from_str(&report.to_json_string()).unwrap();
        assert_eq!(json["schema_version"], 1);
        assert_eq!(json["diagnostics"][0]["suggestion"]["edit"]["start_byte"], start);
        assert_eq!(json["diagnostics"][0]["suggestion"]["edit"]["new_text"], "counter");
        clear_source_map();
    }

    #[test]
    fn test_ambiguous_unbound_variable_does_not_offer_edit() {
        let source = "let count = 1\nlet value = countr\n";
        set_source_map_with_file(source, Some("ambiguous.nula"));
        let start = source.find("countr").unwrap() as u32;
        let error = NuError::unbound_variable(
            "countr",
            Span::new(start, start + 6),
            Some(vec!["counter".into(), "count".into()]),
        );

        assert!(diagnostics_from_error(&error)[0].suggestion.is_none());
        clear_source_map();
    }

    #[test]
    fn test_unbound_variable_mismatched_or_missing_source_never_offers_edit() {
        let source = "let value = different\n";
        set_source_map_with_file(source, Some("mismatch.nula"));
        let start = source.find("different").unwrap() as u32;
        let error = NuError::unbound_variable(
            "countr",
            Span::new(start, start + "different".len() as u32),
            Some(vec!["counter".into()]),
        );
        let diagnostic = &diagnostics_from_error(&error)[0];
        assert!(diagnostic.suggestion.as_ref().and_then(|s| s.edit.as_ref()).is_none());
        clear_source_map();

        assert!(diagnostics_from_error(&error)[0]
            .suggestion
            .as_ref()
            .and_then(|s| s.edit.as_ref())
            .is_none());
    }

    #[test]
    fn test_unbound_variable_edit_offsets_are_utf8_bytes() {
        let source = "let café = 1\nlet output = cafè\n";
        set_source_map_with_file(source, Some("unicode.nula"));
        let start = source.find("cafè").unwrap() as u32;
        let end = start + "cafè".len() as u32;
        let error = NuError::unbound_variable(
            "cafè",
            Span::new(start, end),
            Some(vec!["café".into()]),
        );

        let diagnostics = diagnostics_from_error(&error);
        let edit = diagnostics[0].suggestion.as_ref().unwrap().edit.as_ref().unwrap();
        assert_eq!(edit.start_byte, start);
        assert_eq!(edit.end_byte, end);
        assert_eq!(&source[edit.start_byte as usize..edit.end_byte as usize], "cafè");
        assert_eq!(edit.new_text, "café");
        clear_source_map();
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
