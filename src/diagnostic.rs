//! Rich diagnostic rendering for [`NuError`] using the `ariadne` crate.
//!
//! Two surfaces live here:
//!
//! 1. **Stable error codes** — [`NuError::stable_code`] assigns every error
//!    variant a category-scoped code (`E0101`-style; see
//!    `docs/ERROR_CODES.md`). Codes are shown in report headers as
//!    `error[E0201]: ...`.
//!
//! 2. **Ariadne report rendering** — [`render`] builds a full source-snippet
//!    report (carets, labels, notes, help) from the thread-local
//!    [`SourceMap`](crate::types::SourceMap) installed by the lexer. When no
//!    source is available, [`render`] returns `None` and callers fall back to
//!    [`format_diagnostic`], which emits a plain-text Rust-style report.
//!
//! [`format_diagnostic`] is the canonical human-facing entry point: it returns
//! a source-snippet report when a source map is installed and a plain fallback
//! otherwise, so every CLI/REPL/package error is rendered consistently.

use ariadne::{Color, Config, Label, Report, ReportKind, Source};

use crate::types::{current_source_text, source_map_file, ErrorCode, NuError, NuWarning, Span};

impl NuError {
    /// Return the stable, category-scoped diagnostic code for this error.
    ///
    /// Numbering scheme (documented in `docs/ERROR_CODES.md`):
    ///
    /// - `E01xx` — lexing/parsing
    /// - `E02xx` — type checking
    /// - `E03xx` — effects
    /// - `E04xx` — reference capabilities
    /// - `E05xx` — runtime / VM
    /// - `E06xx` — foreign interfaces (FFI, Python interop)
    /// - `E09xx` — miscellaneous (not-yet-implemented, packaging)
    ///
    /// Returns `None` for [`NuError::Suspended`] (not an error) and
    /// [`NuError::Multiple`] (each child carries its own code).
    pub fn stable_code(&self) -> Option<&'static str> {
        // Prefer the fine-grained classification from `error_code()` (which
        // inspects structured fields and message patterns), remapped onto
        // the category-scoped scheme.
        if let Some(legacy) = self.error_code() {
            return Some(match legacy {
                ErrorCode::E001UnclosedDelimiter => "E0103",
                ErrorCode::E002UnboundVariable => "E0202",
                ErrorCode::E003TypeMismatch => "E0201",
                ErrorCode::E004MissingEffect => "E0301",
                ErrorCode::E005SendabilityViolation => "E0401",
                ErrorCode::E006LinearUseAfterConsume => "E0402",
                ErrorCode::E007InfiniteType => "E0203",
                ErrorCode::E008FieldNotFound => "E0204",
                ErrorCode::E009WrongArity => "E0205",
                ErrorCode::E010MatchNoArms => "E0206",
                ErrorCode::E011StepLimitExceeded => "E0503",
                ErrorCode::E012UnhandledEffect => "E0302",
                ErrorCode::E013FfiBoundaryViolation => "E0208",
            });
        }
        // Fall back to a per-variant category default.
        match self {
            NuError::LexError { .. } => Some("E0101"),
            NuError::ParseError { .. } => Some("E0102"),
            NuError::TypeError { .. } => Some("E0200"),
            NuError::EffectError { .. } => Some("E0300"),
            NuError::CapError { .. } => Some("E0400"),
            NuError::FFIError { .. } => Some("E0601"),
            NuError::PythonError { .. } => Some("E0602"),
            NuError::NotYetImplemented { .. } => Some("E0901"),
            NuError::RuntimeError { .. } => Some("E0501"),
            NuError::VMError { .. } => Some("E0502"),
            NuError::PackageError { .. } => Some("E0902"),
            NuError::Suspended(_) | NuError::Multiple(_) => None,
        }
    }

    /// The primary source span of this error, if the variant carries one.
    /// Public so the JSON diagnostics view (`crate::json_diagnostics`) can
    /// resolve it to line/col without forking the pipeline.
    pub fn primary_span(&self) -> Option<Span> {
        match self {
            NuError::LexError { span, .. }
            | NuError::ParseError { span, .. }
            | NuError::TypeError { span, .. }
            | NuError::EffectError { span, .. }
            | NuError::CapError { span, .. }
            | NuError::FFIError { span, .. }
            | NuError::NotYetImplemented { span, .. }
            | NuError::RuntimeError { span, .. }
            | NuError::VMError { span, .. }
            | NuError::PythonError { span, .. }
            | NuError::PackageError { span, .. } => Some(*span),
            NuError::Suspended(_) | NuError::Multiple(_) => None,
        }
    }
}

/// Render `err` as an `ariadne` source-snippet report.
///
/// Returns `None` when rendering is not possible or not meaningful: no
/// thread-local [`SourceMap`](crate::types::SourceMap) is installed (so no
/// source text is available), the error is a [`NuError::Suspended`]
/// notification, or a child of [`NuError::Multiple`] fails to render.
/// Callers should fall back to [`format_diagnostic`] in that case.
///
/// `use_color` maps onto ariadne's color config: pass `false` for non-tty/CI
/// output to get a plain-text snippet report.
pub fn render(err: &NuError, use_color: bool) -> Option<String> {
    match err {
        NuError::Multiple(errors) => {
            let mut out = String::new();
            for e in errors {
                out.push_str(&render(e, use_color)?);
                out.push('\n');
            }
            Some(out)
        }
        NuError::Suspended(_) => None,
        _ => render_single(err, use_color),
    }
}

/// Render a [`NuWarning`] as an `ariadne` source-snippet warning report.
///
/// Returns `None` when no thread-local source is installed; callers should
/// fall back to [`NuWarning::format_plain`].
pub fn render_warning(warning: &NuWarning, use_color: bool) -> Option<String> {
    let source = current_source_text()?;
    if source.is_empty() {
        return None;
    }
    let span = warning.span;
    let file = source_map_file().unwrap_or_else(|| "<input>".to_string());

    let len = source.len();
    let start = (span.start as usize).min(len);
    let end = (span.end as usize).min(len).max(start);
    let range = start..end;

    let mut builder = Report::build(ReportKind::Warning, file.as_str(), start)
        .with_config(Config::default().with_color(use_color))
        .with_message(&warning.msg)
        .with_code(warning.code)
        .with_label(
            Label::new((file.as_str(), range))
                .with_message(&warning.msg)
                .with_color(Color::Yellow),
        );
    if let Some(help) = &warning.help {
        builder = builder.with_help(help);
    }

    let mut out: Vec<u8> = Vec::new();
    builder
        .finish()
        .write((file.as_str(), Source::from(source)), &mut out)
        .ok()?;
    String::from_utf8(out).ok()
}

/// Format a [`NuWarning`] for display: ariadne snippet when a source map is
/// installed, otherwise the plain one-line form.
pub fn format_warning(warning: &NuWarning, use_color: bool) -> String {
    render_warning(warning, use_color).unwrap_or_else(|| warning.format_plain())
}

/// Format an [`NuError`] for human display.
///
/// Returns an `ariadne` source-snippet report when a thread-local source map is
/// installed, otherwise a plain-text Rust-style report (`error[E0201]: ...`,
/// `--> file:line:col`, notes, and a `help:` line). This is the canonical
/// entry point for all terminal/CLI/REPL/package error output.
pub fn format_diagnostic(err: &NuError, use_color: bool) -> String {
    render(err, use_color).unwrap_or_else(|| format_plain_diagnostic(err))
}

/// Plain-text Rust-style fallback when no source map is available.
///
/// Emits `error[CODE]: message`, the source location when resolvable, the
/// structured notes (`expected type:`, `found type:`, `missing effects:`,
/// `did you mean one of:`, etc.), and any suggestion as a `help:` line.
fn format_plain_diagnostic(err: &NuError) -> String {
    match err {
        NuError::Multiple(errors) => errors
            .iter()
            .map(|e| format_plain_diagnostic(e))
            .collect::<Vec<_>>()
            .join("\n"),
        NuError::Suspended(kind) => format!("info: VM suspended ({kind})"),
        _ => {
            let mut out = String::new();
            let msg = err.to_string_message();
            if let Some(code) = err.stable_code() {
                out.push_str(&format!("error[{code}]: {msg}\n"));
            } else {
                out.push_str(&format!("error: {msg}\n"));
            }
            if let Some(span) = err.primary_span() {
                let line = span.line();
                if line > 0 {
                    let file = span.file().unwrap_or_else(|| "<input>".to_string());
                    let col = span.column();
                    out.push_str(&format!("  --> {file}:{line}:{col}\n"));
                }
            }
            for note in diagnostic_notes(err) {
                out.push_str(&format!("  = note: {note}\n"));
            }
            if let Some(help) = err.suggestion() {
                out.push_str(&format!("  = help: {help}\n"));
            }
            out
        }
    }
}

/// Render a single (non-`Multiple`) error.
fn render_single(err: &NuError, use_color: bool) -> Option<String> {
    let source = current_source_text()?;
    if source.is_empty() {
        return None;
    }
    let span = err.primary_span()?;
    let file = source_map_file().unwrap_or_else(|| "<input>".to_string());

    // Clamp the byte range to the source so ariadne never sees an
    // out-of-bounds span (synthetic spans like `Span::default()` degrade to
    // a zero-width marker at offset 0).
    let len = source.len();
    let start = (span.start as usize).min(len);
    let end = (span.end as usize).min(len).max(start);
    let range = start..end;

    let msg = err.to_string_message();
    let mut builder = Report::build(ReportKind::Error, file.as_str(), start)
        .with_config(Config::default().with_color(use_color))
        .with_message(&msg)
        .with_label(
            Label::new((file.as_str(), range))
                .with_message(&msg)
                .with_color(Color::Red),
        );
    if let Some(code) = err.stable_code() {
        builder = builder.with_code(code);
    }
    let notes = diagnostic_notes(err);
    if !notes.is_empty() {
        builder = builder.with_note(notes.join("; "));
    }
    if let Some(help) = err.suggestion() {
        builder = builder.with_help(help);
    }

    let mut out: Vec<u8> = Vec::new();
    builder
        .finish()
        .write((file.as_str(), Source::from(source)), &mut out)
        .ok()?;
    String::from_utf8(out).ok()
}

/// Extract just the core message (no position prefix or structured-field
/// suffixes) for use as the ariadne label message.
impl NuError {
    fn to_string_message(&self) -> String {
        match self {
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
}

/// Build the structured-field note lines for an error (expected/found types,
/// missing effects, capability explanations, similar-name suggestions).
/// These mirror the extra lines emitted by `NuError`'s `Display` so no
/// diagnostic content is lost in the ariadne path.
pub fn diagnostic_notes(err: &NuError) -> Vec<String> {
    let mut notes = Vec::new();
    match err {
        NuError::ParseError {
            expected, found, ..
        } => {
            if let Some(exp) = expected {
                notes.push(format!("expected: {exp}"));
            }
            if let Some(fnd) = found {
                notes.push(format!("found: {fnd}"));
            }
        }
        NuError::TypeError {
            expected_type,
            found_type,
            similar_names,
            ..
        } => {
            if let Some(exp) = expected_type {
                notes.push(format!("expected type: {exp}"));
            }
            if let Some(fnd) = found_type {
                notes.push(format!("found type: {fnd}"));
            }
            if let Some(names) = similar_names {
                if !names.is_empty() {
                    notes.push(format!("did you mean one of: {}?", names.join(", ")));
                }
            }
        }
        NuError::EffectError {
            missing_effects,
            allowed_effects,
            ..
        } => {
            if let Some(missing) = missing_effects {
                if !missing.is_empty() {
                    notes.push(format!("missing effects: {}", missing.join(", ")));
                }
            }
            if let Some(allowed) = allowed_effects {
                notes.push(format!("allowed effects: {allowed}"));
            }
        }
        NuError::CapError { explanation, .. } => {
            if let Some(expl) = explanation {
                notes.push(format!("note: {expl}"));
            }
        }
        _ => {}
    }
    notes
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::types::{clear_source_map, set_source_map_with_file, Span};

    /// Install a source map for the duration of a test and clear it after.
    /// Tests run on separate threads, so the thread-local map does not race.
    fn with_source(source: &str, f: impl FnOnce()) {
        set_source_map_with_file(source, Some("test.nula"));
        f();
        clear_source_map();
    }

    /// Offset of the first occurrence of `needle` in `source`.
    fn offset_of(source: &str, needle: &str) -> usize {
        source.find(needle).expect("needle must exist in source")
    }

    // -- stable codes -----------------------------------------------------

    #[test]
    fn test_stable_code_categories_by_variant() {
        let span = Span::default();
        assert_eq!(
            NuError::LexError {
                msg: "bad char".into(),
                span
            }
            .stable_code(),
            Some("E0101")
        );
        assert_eq!(
            NuError::parse_error("oops".into(), span).stable_code(),
            Some("E0102")
        );
        assert_eq!(
            NuError::effect_error("nope".into(), span).stable_code(),
            Some("E0300")
        );
        assert_eq!(
            NuError::cap_error("bad cap".into(), span).stable_code(),
            Some("E0400")
        );
        assert_eq!(
            NuError::runtime_error("boom".into(), span).stable_code(),
            Some("E0501")
        );
        assert_eq!(
            NuError::vm_error("boom".into(), span).stable_code(),
            Some("E0502")
        );
        assert_eq!(
            NuError::ffi_error("bad ffi".into(), span).stable_code(),
            Some("E0601")
        );
        assert_eq!(
            NuError::Suspended(crate::types::VmSuspension::SignalWait).stable_code(),
            None
        );
        assert_eq!(
            NuError::Multiple(vec![NuError::vm_error("x".into(), span)]).stable_code(),
            None
        );
    }

    #[test]
    fn test_stable_code_prefers_fine_grained_classification() {
        let span = Span::default();
        let mismatch = NuError::type_mismatch("Int", "String", span);
        assert_eq!(mismatch.stable_code(), Some("E0201"));
        let unbound = NuError::unbound_variable("fooo", span, None);
        assert_eq!(unbound.stable_code(), Some("E0202"));
        let missing = NuError::missing_effects(vec!["IO".to_string()], "{}", span);
        assert_eq!(missing.stable_code(), Some("E0301"));
    }

    // -- ariadne rendering (snapshot-style) --------------------------------

    #[test]
    fn test_render_parse_error_snapshot() {
        let source = "fn main() {\n    let x = [1, 2;\n}\n";
        with_source(source, || {
            let start = offset_of(source, ";") as u32;
            let err = NuError::ParseError {
                msg: "Expected ']'".to_string(),
                span: Span::new(start, start + 1),
                expected: Some("']'".to_string()),
                found: Some("';'".to_string()),
            };
            let rendered = render(&err, false).expect("should render with source");
            assert_eq!(
                rendered,
                "[E0102] Error: Expected ']'\n   ╭─[test.nula:2:18]\n   │\n 2 │     let x = [1, 2;\n   │                  ┬  \n   │                  ╰── Expected ']'\n   │ \n   │ Help: unclosed bracket — add a `]` to close the list or array type\n   │ \n   │ Note: expected: ']'; found: ';'\n───╯\n"
            );
            // Plain Display output is unchanged (no code, plain prefix).
            assert!(format!("{err}").starts_with("Parse error at 2:18:"));
        });
    }

    #[test]
    fn test_render_type_mismatch_snapshot() {
        let source = "fn add(a: Int, b: Int) -> Int = a + b\nfn main() = add(1, \"two\")\n";
        with_source(source, || {
            let start = offset_of(source, "\"two\"") as u32;
            let err = NuError::TypeError {
                msg: "Cannot unify Int with String".to_string(),
                span: Span::new(start, start + 5),
                expected_type: Some("Int".to_string()),
                found_type: Some("String".to_string()),
                similar_names: None,
            };
            let rendered = render(&err, false).expect("should render with source");
            assert_eq!(
                rendered,
                "[E0201] Error: Cannot unify Int with String\n   ╭─[test.nula:2:20]\n   │\n 2 │ fn main() = add(1, \"two\")\n   │                    ──┬──  \n   │                      ╰──── Cannot unify Int with String\n   │ \n   │ Help: the expression produces the wrong type — consider adding a type annotation or conversion\n   │ \n   │ Note: expected type: Int; found type: String\n───╯\n"
            );
        });
    }

    #[test]
    fn test_render_effect_error_snapshot() {
        let source = "fn greet() -> Unit = perform IO.print(\"hi\")\n";
        with_source(source, || {
            let start = offset_of(source, "perform") as u32;
            let err = NuError::EffectError {
                msg: "effects {IO} are not a subset of allowed effects {}".to_string(),
                span: Span::new(start, start + 7),
                missing_effects: Some(vec!["IO".to_string()]),
                allowed_effects: Some("{}".to_string()),
            };
            let rendered = render(&err, false).expect("should render with source");
            assert!(
                rendered.starts_with("[E0301] Error:"),
                "header: {rendered:?}"
            );
            assert!(rendered.contains("perform"));
            assert!(rendered.contains("missing effects: IO"));
            assert!(rendered.contains("allowed effects: {}"));
        });
    }

    #[test]
    fn test_render_unbound_variable_with_suggestion() {
        let source = "fn main() = countr + 1\n";
        with_source(source, || {
            let start = offset_of(source, "countr") as u32;
            let err = NuError::unbound_variable(
                "countr",
                Span::new(start, start + 6),
                Some(vec!["counter".to_string()]),
            );
            let rendered = render(&err, false).expect("should render with source");
            assert!(
                rendered.starts_with("[E0202] Error:"),
                "header: {rendered:?}"
            );
            assert!(rendered.contains("Unbound variable"));
            assert!(rendered.contains("did you mean one of: counter?"));
        });
    }

    #[test]
    fn test_render_returns_none_without_source_map() {
        clear_source_map();
        let err = NuError::vm_error("boom".into(), Span::default());
        assert!(render(&err, false).is_none());
        assert!(render(&err, true).is_none());
    }

    #[test]
    fn test_render_suspended_returns_none() {
        let err = NuError::Suspended(crate::types::VmSuspension::ReceiveWait);
        assert!(render(&err, false).is_none());
    }

    // -- warnings (RFC 0015 deprecations) -----------------------------------

    #[test]
    fn test_render_warning_snapshot() {
        let source = "let port = catch parse_port(env) 8080\n";
        with_source(source, || {
            let start = offset_of(source, "catch") as u32;
            let w = NuWarning::deprecated_catch(Span::new(start, start + 5));
            let rendered = render_warning(&w, false).expect("should render with source");
            assert!(
                rendered.starts_with("[W0101] Warning:"),
                "header: {rendered:?}"
            );
            assert!(rendered.contains("deprecated `catch`"));
            assert!(rendered.contains("RFC 0015"), "help line: {rendered:?}");
        });
    }

    #[test]
    fn test_format_warning_plain_fallback_without_source() {
        clear_source_map();
        let w = NuWarning::deprecated_fail(Span::new(0, 4));
        let plain = format_warning(&w, false);
        assert_eq!(
            plain,
            "warning[W0102]: use of deprecated `fail` expression\n  = help: `fail` is deprecated by RFC 0015 and will be removed in v2.0 — use `return` with an explicit `Error(...)` value under a `T ! E` signature"
        );
    }

    #[test]
    fn test_render_warning_returns_none_without_source_map() {
        clear_source_map();
        let w = NuWarning::deprecated_catch(Span::default());
        assert!(render_warning(&w, false).is_none());
    }

    #[test]
    fn test_render_multiple_renders_each_child() {
        let source = "fn main() = @\n";
        with_source(source, || {
            let at = offset_of(source, "@") as u32;
            let errs = NuError::Multiple(vec![
                NuError::LexError {
                    msg: "unexpected character '@'".to_string(),
                    span: Span::new(at, at + 1),
                },
                NuError::parse_error("Unexpected end of file".to_string(), Span::new(at, at + 1)),
            ]);
            let rendered = render(&errs, false).expect("should render with source");
            assert!(
                rendered.contains("[E0101] Error:"),
                "lex code: {rendered:?}"
            );
            assert!(
                rendered.contains("[E0102] Error:"),
                "parse code: {rendered:?}"
            );
        });
    }

    // -- canonical format_diagnostic entry point ---------------------------

    #[test]
    fn test_format_diagnostic_uses_ariadne_with_source_map() {
        let source = "fn main() = countr + 1\n";
        with_source(source, || {
            let start = offset_of(source, "countr") as u32;
            let err = NuError::unbound_variable(
                "countr",
                Span::new(start, start + 6),
                Some(vec!["counter".to_string()]),
            );
            let rendered = format_diagnostic(&err, false);
            assert!(
                rendered.contains("[E0202] Error:"),
                "should contain stable code header: {rendered:?}"
            );
            assert!(
                rendered.contains("countr"),
                "should contain source span: {rendered:?}"
            );
            assert!(
                rendered.contains("did you mean one of: counter?"),
                "should contain suggestion note: {rendered:?}"
            );
        });
    }

    #[test]
    fn test_format_diagnostic_plain_fallback_without_source_map() {
        clear_source_map();
        let err = NuError::type_mismatch("Int", "String", Span::default());
        let rendered = format_diagnostic(&err, false);
        assert!(
            rendered.starts_with("error[E0201]:"),
            "should start with rustc-style header: {rendered:?}"
        );
        assert!(
            rendered.contains("expected type: Int"),
            "should contain expected type: {rendered:?}"
        );
        assert!(
            rendered.contains("found type: String"),
            "should contain found type: {rendered:?}"
        );
        assert!(
            rendered.contains("= help:"),
            "should contain help line: {rendered:?}"
        );
    }

    #[test]
    fn test_format_diagnostic_plain_effect_error() {
        clear_source_map();
        let err = NuError::missing_effects(vec!["IO".to_string()], "{}", Span::default());
        let rendered = format_diagnostic(&err, false);
        assert!(
            rendered.contains("missing effects: IO"),
            "should contain missing effects: {rendered:?}"
        );
        assert!(
            rendered.contains("allowed effects: {}"),
            "should contain allowed effects: {rendered:?}"
        );
    }

    #[test]
    fn test_format_diagnostic_multiple_renders_each_child() {
        clear_source_map();
        let errs = NuError::Multiple(vec![
            NuError::vm_error("boom".into(), Span::default()),
            NuError::parse_error("oops".into(), Span::default()),
        ]);
        let rendered = format_diagnostic(&errs, false);
        assert!(rendered.contains("error[E0502]:"), "vm code: {rendered:?}");
        assert!(
            rendered.contains("error[E0102]:"),
            "parse code: {rendered:?}"
        );
    }

    #[test]
    fn test_format_diagnostic_suspended_returns_info() {
        clear_source_map();
        let err = NuError::Suspended(crate::types::VmSuspension::SignalWait);
        let rendered = format_diagnostic(&err, false);
        assert!(rendered.contains("VM suspended"), "{rendered:?}");
    }
}
