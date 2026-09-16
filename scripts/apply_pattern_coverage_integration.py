#!/usr/bin/env python3
from pathlib import Path


def replace_once(path: str, old: str, new: str) -> None:
    p = Path(path)
    text = p.read_text()
    count = text.count(old)
    if count != 1:
        raise SystemExit(f"{path}: expected exactly one anchor, found {count}: {old[:120]!r}")
    p.write_text(text.replace(old, new, 1))


# ---------------------------------------------------------------------------
# TypeChecker: collect finite-pattern diagnostics at the point where the
# scrutinee's fully-substituted type is authoritative.
# ---------------------------------------------------------------------------
replace_once(
    "src/typechecker.rs",
    """    /// Errors collected when `collect_errors` is set (empty otherwise).\n    pub collected_errors: Vec<crate::types::NuError>,\n}\n""",
    """    /// Errors collected when `collect_errors` is set (empty otherwise).\n    pub collected_errors: Vec<crate::types::NuError>,\n    /// Non-fatal semantic diagnostics collected during type inference.\n    ///\n    /// Pattern-coverage diagnostics live here rather than in the parser so\n    /// they are based on the fully inferred scrutinee type.\n    pub warnings: Vec<NuWarning>,\n}\n""",
)

replace_once(
    "src/typechecker.rs",
    """            rigid_vars: FxHashSet::default(),\n            collect_errors: false,\n            collected_errors: Vec::new(),\n        }\n    }\n\n    /// Type-check an entire module, returning the type of the last declaration.\n""",
    """            rigid_vars: FxHashSet::default(),\n            collect_errors: false,\n            collected_errors: Vec::new(),\n            warnings: Vec::new(),\n        }\n    }\n\n    /// Consume and return non-fatal semantic warnings collected by the last\n    /// type-checking operation.\n    pub fn take_warnings(&mut self) -> Vec<NuWarning> {\n        std::mem::take(&mut self.warnings)\n    }\n\n    /// Type-check an entire module, returning the type of the last declaration.\n""",
)

replace_once(
    "src/typechecker.rs",
    """        Ok((final_subst.clone(), apply_subst(&first_arm, &final_subst)))\n    }\n\n    /// Bind pattern variables into a new context.\n""",
    """        // Coverage is a semantic diagnostic, not a validity rule in Nulang\n        // 1.x: RFC 0002 freezes existing Core `match` program validity. Run\n        // the conservative finite-variant analysis only after all ordinary\n        // pattern/guard/arm type inference has succeeded, using the fully\n        // substituted scrutinee type as the source of truth.\n        let coverage_ty = apply_subst(&scrut_ty, &final_subst);\n        let coverage_arms: Vec<(Pattern, bool)> = arms\n            .iter()\n            .map(|(pattern, guard, _)| (pattern.clone(), guard.is_some()))\n            .collect();\n        self.warnings.extend(\n            crate::pattern_coverage::warnings_for_variant_match(\n                &coverage_ty,\n                &coverage_arms,\n                span,\n            ),\n        );\n\n        Ok((final_subst.clone(), apply_subst(&first_arm, &final_subst)))\n    }\n\n    /// Bind pattern variables into a new context.\n""",
)

# ---------------------------------------------------------------------------
# CLI: surface typechecker warnings through the same rendering and
# --deny-warnings policy already used for parser deprecations.
# ---------------------------------------------------------------------------
replace_once(
    "src/main.rs",
    """    // 3. Type check\n    let mut type_checker = TypeChecker::new();\n    let module_type = type_checker.check_module(&ast)?;\n\n    if verbose {\n""",
    """    // 3. Type check\n    let mut type_checker = TypeChecker::new();\n    let module_type = type_checker.check_module(&ast)?;\n\n    // Semantic warnings are emitted only after successful type inference so\n    // they can use resolved variant types. They follow the same compatibility\n    // contract as parser warnings: warning-by-default, strict under\n    // --deny-warnings.\n    let type_warnings = type_checker.take_warnings();\n    if !type_warnings.is_empty() {\n        let use_color = std::io::stderr().is_terminal();\n        for w in &type_warnings {\n            eprintln!(\"{}\", nulang::diagnostic::format_warning(w, use_color));\n        }\n        if deny_warnings {\n            return Err(nulang::types::NuError::parse_error(\n                format!(\n                    \"aborting due to {} warning{} (--deny-warnings)\",\n                    type_warnings.len(),\n                    if type_warnings.len() == 1 { \"\" } else { \"s\" }\n                ),\n                type_warnings[0].span,\n            ));\n        }\n    }\n\n    if verbose {\n""",
)

# ---------------------------------------------------------------------------
# LSP: retain warning code/severity and source range instead of flattening the
# new diagnostics into generic effect/capability strings.
# ---------------------------------------------------------------------------
replace_once(
    "src/lsp/mod.rs",
    """/// Convert a `NuError` into zero or more LSP `Diagnostic`s.\n/// For `NuError::Multiple`, each sub-error becomes its own diagnostic.\nfn nu_error_to_diagnostic(err: NuError) -> Vec<Diagnostic> {\n""",
    """/// Convert a compiler warning into an LSP diagnostic while preserving its\n/// stable warning code and source span.\nfn nu_warning_to_diagnostic(warning: crate::types::NuWarning) -> Diagnostic {\n    let span = warning.span;\n    let message = match warning.help {\n        Some(help) => format!(\"{}\\nhelp: {}\", warning.msg, help),\n        None => warning.msg,\n    };\n    Diagnostic {\n        range: Range::new(\n            Position::new(\n                span.line().saturating_sub(1) as u32,\n                span.column().saturating_sub(1) as u32,\n            ),\n            Position::new(\n                span.end_line().saturating_sub(1) as u32,\n                span.end_column().saturating_sub(1) as u32,\n            ),\n        ),\n        severity: Some(DiagnosticSeverity::WARNING),\n        code: Some(NumberOrString::String(warning.code.to_string())),\n        code_description: None,\n        source: Some(\"nulang\".to_string()),\n        message,\n        related_information: None,\n        tags: None,\n        data: None,\n    }\n}\n\n/// Convert a `NuError` into zero or more LSP `Diagnostic`s.\n/// For `NuError::Multiple`, each sub-error becomes its own diagnostic.\nfn nu_error_to_diagnostic(err: NuError) -> Vec<Diagnostic> {\n""",
)

replace_once(
    "src/lsp/mod.rs",
    """        let mut tc = TypeChecker::new();\n        tc.collect_errors = true;\n        let _ = tc.check_module(&ast);\n        for err in tc.collected_errors {\n""",
    """        let mut tc = TypeChecker::new();\n        tc.collect_errors = true;\n        let _ = tc.check_module(&ast);\n        for warning in tc.take_warnings() {\n            diagnostics.push(nu_warning_to_diagnostic(warning));\n        }\n        for err in tc.collected_errors {\n""",
)

# ---------------------------------------------------------------------------
# Changelog: record the compatibility-preserving diagnostic behavior.
# ---------------------------------------------------------------------------
replace_once(
    "CHANGELOG.md",
    """### Added since 1.0.0-frozen — 2026-09-14 (web contract + capacity broker hardening)\n""",
    """### Added since 1.0.0-frozen — 2026-09-16 (pattern coverage diagnostics)\n- **Finite variant match coverage diagnostics** (Experimental, RFC 0020,\n  `src/pattern_coverage.rs`, `src/typechecker.rs`). After successful ordinary\n  type inference, matches over statically finite variant types emit `W0201`\n  for missing constructors and `W0202` for redundant arms. Guarded arms do\n  not prove exhaustiveness. Existing Nulang Core program validity and the\n  runtime non-exhaustive-match fallback remain unchanged;\n  `--deny-warnings` provides opt-in strictness. The LSP preserves the same\n  warning codes and source ranges.\n\n### Added since 1.0.0-frozen — 2026-09-14 (web contract + capacity broker hardening)\n""",
)

# ---------------------------------------------------------------------------
# Integration tests exercise the real lexer/parser/typechecker seam rather
# than only the raw coverage helper.
# ---------------------------------------------------------------------------
test_path = Path("tests/pattern_coverage_diagnostics.rs")
if test_path.exists():
    raise SystemExit(f"{test_path}: already exists")
test_path.write_text(r'''use nulang::lexer::Lexer;
use nulang::parser::Parser;
use nulang::typechecker::TypeChecker;
use nulang::types::NuWarning;

fn warnings_for(source: &str) -> Vec<NuWarning> {
    let tokens = Lexer::new(source).lex().expect("lex");
    let mut parser = Parser::new(tokens);
    let ast = parser.parse_module().expect("parse");
    let mut type_checker = TypeChecker::new();
    type_checker.check_module(&ast).expect("typecheck");
    type_checker.take_warnings()
}

#[test]
fn non_exhaustive_variant_match_emits_w0201() {
    let warnings = warnings_for(
        r#"
        type Color = Red | Green | Blue
        fn name(c: Color) -> String {
            match c {
                | Red => "red"
                | Green => "green"
            }
        }
        "#,
    );

    assert_eq!(warnings.len(), 1);
    assert_eq!(warnings[0].code, "W0201");
    assert!(warnings[0].msg.contains("Blue"));
}

#[test]
fn exhaustive_variant_match_has_no_coverage_warning() {
    let warnings = warnings_for(
        r#"
        type Color = Red | Green | Blue
        fn name(c: Color) -> String {
            match c {
                | Red => "red"
                | Green => "green"
                | Blue => "blue"
            }
        }
        "#,
    );

    assert!(warnings.is_empty(), "unexpected warnings: {warnings:?}");
}

#[test]
fn redundant_variant_arm_emits_w0202() {
    let warnings = warnings_for(
        r#"
        type Color = Red | Green
        fn warm(c: Color) -> Int {
            match c {
                | Red => 1
                | Red => 2
                | _ => 0
            }
        }
        "#,
    );

    assert!(warnings.iter().any(|w| w.code == "W0202"));
}
''')

print("pattern coverage integration patch applied")
