//! Conservative static coverage analysis for `match` expressions.
//!
//! The runtime already retains a non-exhaustive-match fallback for cases the
//! compiler cannot prove. This module handles decidable finite domains first:
//! declared variants and booleans. It deliberately prefers false negatives
//! (leave the runtime fallback in place) over false positives (claim an arm is
//! exhaustive when it is not).
//!
//! Nulang Core freezes the validity and runtime behavior of existing `match`
//! programs (RFC 0002), so coverage findings are warnings rather than new hard
//! type errors. Projects that want strict matching can opt into the existing
//! `--deny-warnings` policy once these diagnostics are surfaced by the frontend.

use crate::ast::{Literal, Pattern};
use crate::types::{NuWarning, PrimitiveType, Span, Type};
use std::collections::HashSet;

/// Result of statically analysing a finite match.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct CoverageReport {
    /// Human-readable witnesses for values not covered by any unguarded arm.
    pub missing: Vec<String>,
    /// Zero-based arm indexes that can never be reached because an earlier
    /// unguarded arm already covers the same value/constructor (or all values).
    pub redundant_arms: Vec<usize>,
}

impl CoverageReport {
    pub fn is_exhaustive(&self) -> bool {
        self.missing.is_empty()
    }
}

/// Analyse a match when its scrutinee belongs to a finite domain currently
/// understood by the compiler coverage engine.
///
/// Supported today:
/// - declared variant types;
/// - `Bool`.
///
/// `arms` stores the pattern and whether that arm has a guard. Guarded arms do
/// not contribute to exhaustiveness because the guard may evaluate to false.
/// Returns `None` for domains the analyzer cannot prove finite/complete; those
/// continue to rely on the existing runtime non-exhaustive fallback.
pub fn analyze_match(scrutinee_ty: &Type, arms: &[(Pattern, bool)]) -> Option<CoverageReport> {
    match peel_reference(scrutinee_ty) {
        Type::Variant(_) => analyze_variant_match(scrutinee_ty, arms),
        Type::Primitive(PrimitiveType::Bool) => Some(analyze_bool_match(arms)),
        _ => None,
    }
}

/// Analyse a match when its scrutinee has a statically finite variant type.
pub fn analyze_variant_match(
    scrutinee_ty: &Type,
    arms: &[(Pattern, bool)],
) -> Option<CoverageReport> {
    let Type::Variant(variants) = peel_reference(scrutinee_ty) else {
        return None;
    };

    let declared: HashSet<&str> = variants.iter().map(|(name, _)| name.as_str()).collect();
    let mut covered: HashSet<String> = HashSet::new();
    let mut redundant_arms = Vec::new();
    let mut catch_all = false;

    for (index, (pattern, guarded)) in arms.iter().enumerate() {
        if catch_all {
            redundant_arms.push(index);
            continue;
        }

        // A guarded arm can reject at runtime, so it never closes a coverage
        // hole. It can still be unreachable after an earlier unguarded arm,
        // which is handled above and by the constructor check below.
        if *guarded {
            if let Some(name) = covered_constructor(pattern, variants) {
                if covered.contains(name) {
                    redundant_arms.push(index);
                }
            }
            continue;
        }

        if is_unconditional_catch_all(pattern) {
            catch_all = true;
            covered.extend(declared.iter().map(|name| (*name).to_string()));
            continue;
        }

        if let Some(name) = covered_constructor(pattern, variants) {
            if covered.contains(name) {
                redundant_arms.push(index);
            } else {
                covered.insert(name.to_string());
            }
        }
    }

    let missing = variants
        .iter()
        .filter(|(name, _)| !covered.contains(name))
        .map(|(name, payload)| match payload {
            Some(_) => format!("{}(_)", name),
            None => name.clone(),
        })
        .collect();

    Some(CoverageReport {
        missing,
        redundant_arms,
    })
}

/// Analyse the two-value boolean domain.
fn analyze_bool_match(arms: &[(Pattern, bool)]) -> CoverageReport {
    let mut covered_true = false;
    let mut covered_false = false;
    let mut catch_all = false;
    let mut redundant_arms = Vec::new();

    for (index, (pattern, guarded)) in arms.iter().enumerate() {
        if catch_all {
            redundant_arms.push(index);
            continue;
        }

        let pattern = strip_alias(pattern);

        if *guarded {
            match pattern {
                Pattern::Lit(Literal::Bool(true)) if covered_true => redundant_arms.push(index),
                Pattern::Lit(Literal::Bool(false)) if covered_false => redundant_arms.push(index),
                _ => {}
            }
            continue;
        }

        if is_unconditional_catch_all(pattern) {
            catch_all = true;
            covered_true = true;
            covered_false = true;
            continue;
        }

        match pattern {
            Pattern::Lit(Literal::Bool(true)) => {
                if covered_true {
                    redundant_arms.push(index);
                } else {
                    covered_true = true;
                }
            }
            Pattern::Lit(Literal::Bool(false)) => {
                if covered_false {
                    redundant_arms.push(index);
                } else {
                    covered_false = true;
                }
            }
            _ => {}
        }
    }

    let mut missing = Vec::with_capacity(2);
    if !covered_true {
        missing.push("true".to_string());
    }
    if !covered_false {
        missing.push("false".to_string());
    }

    CoverageReport {
        missing,
        redundant_arms,
    }
}

/// Convert finite-domain coverage findings into non-fatal compiler warnings.
///
/// This is intentionally separate from [`analyze_match`] so IDEs, formatters,
/// tests, and future strict-mode frontends can consume the raw report without
/// committing to a presentation policy.
pub fn warnings_for_match(
    scrutinee_ty: &Type,
    arms: &[(Pattern, bool)],
    span: Span,
) -> Vec<NuWarning> {
    let Some(report) = analyze_match(scrutinee_ty, arms) else {
        return Vec::new();
    };

    warnings_from_report(report, span)
}

/// Backward-compatible variant-specific warning helper.
pub fn warnings_for_variant_match(
    scrutinee_ty: &Type,
    arms: &[(Pattern, bool)],
    span: Span,
) -> Vec<NuWarning> {
    let Some(report) = analyze_variant_match(scrutinee_ty, arms) else {
        return Vec::new();
    };

    warnings_from_report(report, span)
}

fn warnings_from_report(report: CoverageReport, span: Span) -> Vec<NuWarning> {
    let mut warnings = Vec::with_capacity(2);

    if !report.missing.is_empty() {
        let missing = report.missing.join(", ");
        warnings.push(NuWarning {
            code: "W0201",
            msg: format!("non-exhaustive match: missing {missing}"),
            span,
            help: Some(format!(
                "add arm{} for {missing}; without one, an uncovered value keeps the frozen runtime non-exhaustive-match behavior",
                if report.missing.len() == 1 { "" } else { "s" }
            )),
        });
    }

    if !report.redundant_arms.is_empty() {
        let one_based: Vec<String> = report
            .redundant_arms
            .iter()
            .map(|index| (index + 1).to_string())
            .collect();
        let label = if one_based.len() == 1 { "arm" } else { "arms" };
        warnings.push(NuWarning {
            code: "W0202",
            msg: format!(
                "redundant match {label}: {} cannot be reached",
                one_based.join(", ")
            ),
            span,
            help: Some(
                "remove the redundant arm or move/refine it before the pattern that already covers the same values"
                    .to_string(),
            ),
        });
    }

    warnings
}

/// Peel compile-time reference wrappers. Pattern matching observes the value
/// shape, not the ownership capability attached to the reference.
fn peel_reference(mut ty: &Type) -> &Type {
    while let Type::Reference { inner, .. } = ty {
        ty = inner;
    }
    ty
}

/// Return the constructor name when this unguarded pattern covers *every*
/// value belonging to that constructor.
fn covered_constructor<'a>(
    pattern: &'a Pattern,
    variants: &'a [(String, Option<Type>)],
) -> Option<&'a str> {
    let pattern = strip_alias(pattern);
    let Pattern::Variant(name, payload_pattern) = pattern else {
        return None;
    };
    let (_, declared_payload) = variants.iter().find(|(ctor, _)| ctor == name)?;

    match (declared_payload, payload_pattern) {
        (None, None) => Some(name.as_str()),
        // For the first coverage slice, only wildcard/variable (possibly
        // aliased) payloads prove full-constructor coverage. Structured tuple
        // and record payloads remain on the runtime fallback until the
        // typechecker validates their exact shape as part of the future
        // Maranget-style pattern-matrix pass.
        (Some(_), Some(inner)) if is_unconditional_catch_all(inner) => Some(name.as_str()),
        _ => None,
    }
}

fn strip_alias(mut pattern: &Pattern) -> &Pattern {
    while let Pattern::Alias(_, inner) = pattern {
        pattern = inner;
    }
    pattern
}

/// Whether this pattern is an unconditional catch-all independent of input
/// shape. We intentionally do not classify tuple/record patterns here: the
/// current binder is permissive about structured shapes, so doing so could
/// turn an invalid structured pattern into a false exhaustiveness proof.
fn is_unconditional_catch_all(pattern: &Pattern) -> bool {
    match pattern {
        Pattern::Wild | Pattern::Var(_) => true,
        Pattern::Alias(_, inner) => is_unconditional_catch_all(inner),
        Pattern::Lit(_) | Pattern::Tuple(_) | Pattern::Record(_) | Pattern::Variant(_, _) => false,
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::types::Type;

    fn color_type() -> Type {
        Type::Variant(vec![
            ("Red".into(), None),
            ("Green".into(), None),
            ("Blue".into(), None),
        ])
    }

    fn variant(name: &str) -> Pattern {
        Pattern::Variant(name.into(), None)
    }

    #[test]
    fn reports_missing_variant_witnesses() {
        let arms = vec![(variant("Red"), false), (variant("Green"), false)];
        let report = analyze_variant_match(&color_type(), &arms).unwrap();
        assert_eq!(report.missing, vec!["Blue"]);
        assert!(!report.is_exhaustive());
    }

    #[test]
    fn accepts_complete_constructor_coverage() {
        let arms = vec![
            (variant("Red"), false),
            (variant("Green"), false),
            (variant("Blue"), false),
        ];
        let report = analyze_variant_match(&color_type(), &arms).unwrap();
        assert!(report.is_exhaustive());
        assert!(report.redundant_arms.is_empty());
    }

    #[test]
    fn unguarded_wildcard_closes_variant_coverage() {
        let arms = vec![(variant("Red"), false), (Pattern::Wild, false)];
        let report = analyze_variant_match(&color_type(), &arms).unwrap();
        assert!(report.is_exhaustive());
    }

    #[test]
    fn guarded_wildcard_does_not_count_as_exhaustive() {
        let arms = vec![(variant("Red"), false), (Pattern::Wild, true)];
        let report = analyze_variant_match(&color_type(), &arms).unwrap();
        assert_eq!(report.missing, vec!["Green", "Blue"]);
    }

    #[test]
    fn payload_variable_covers_whole_constructor() {
        let option = Type::Variant(vec![
            ("Some".into(), Some(Type::int())),
            ("None".into(), None),
        ]);
        let arms = vec![
            (
                Pattern::Variant(
                    "Some".into(),
                    Some(Box::new(Pattern::Var("value".into()))),
                ),
                false,
            ),
            (Pattern::Variant("None".into(), None), false),
        ];
        let report = analyze_variant_match(&option, &arms).unwrap();
        assert!(report.is_exhaustive());
    }

    #[test]
    fn refutable_payload_does_not_cover_whole_constructor() {
        let option = Type::Variant(vec![
            ("Some".into(), Some(Type::int())),
            ("None".into(), None),
        ]);
        let arms = vec![
            (
                Pattern::Variant(
                    "Some".into(),
                    Some(Box::new(Pattern::Lit(Literal::Int(1)))),
                ),
                false,
            ),
            (Pattern::Variant("None".into(), None), false),
        ];
        let report = analyze_variant_match(&option, &arms).unwrap();
        assert_eq!(report.missing, vec!["Some(_)"]);
    }

    #[test]
    fn structured_payload_is_not_yet_used_as_a_proof() {
        let wrapped = Type::Variant(vec![(
            "Pair".into(),
            Some(Type::Tuple(vec![Type::int(), Type::int()])),
        )]);
        let arms = vec![(
            Pattern::Variant(
                "Pair".into(),
                Some(Box::new(Pattern::Tuple(vec![Pattern::Wild, Pattern::Wild]))),
            ),
            false,
        )];
        let report = analyze_variant_match(&wrapped, &arms).unwrap();
        assert_eq!(report.missing, vec!["Pair(_)"]);
    }

    #[test]
    fn finds_redundant_constructor_arms() {
        let arms = vec![
            (variant("Red"), false),
            (variant("Red"), true),
            (Pattern::Wild, false),
            (variant("Blue"), false),
        ];
        let report = analyze_variant_match(&color_type(), &arms).unwrap();
        assert_eq!(report.redundant_arms, vec![1, 3]);
        assert!(report.is_exhaustive());
    }

    #[test]
    fn boolean_match_requires_both_values() {
        let arms = vec![(Pattern::Lit(Literal::Bool(true)), false)];
        let report = analyze_match(&Type::bool(), &arms).unwrap();
        assert_eq!(report.missing, vec!["false"]);
        assert!(!report.is_exhaustive());
    }

    #[test]
    fn boolean_true_false_is_exhaustive() {
        let arms = vec![
            (Pattern::Lit(Literal::Bool(true)), false),
            (Pattern::Lit(Literal::Bool(false)), false),
        ];
        let report = analyze_match(&Type::bool(), &arms).unwrap();
        assert!(report.is_exhaustive());
        assert!(report.redundant_arms.is_empty());
    }

    #[test]
    fn guarded_boolean_arm_does_not_close_coverage() {
        let arms = vec![
            (Pattern::Lit(Literal::Bool(true)), true),
            (Pattern::Lit(Literal::Bool(false)), false),
        ];
        let report = analyze_match(&Type::bool(), &arms).unwrap();
        assert_eq!(report.missing, vec!["true"]);
    }

    #[test]
    fn wildcard_closes_boolean_coverage_and_makes_later_arm_redundant() {
        let arms = vec![
            (Pattern::Wild, false),
            (Pattern::Lit(Literal::Bool(false)), false),
        ];
        let report = analyze_match(&Type::bool(), &arms).unwrap();
        assert!(report.is_exhaustive());
        assert_eq!(report.redundant_arms, vec![1]);
    }

    #[test]
    fn duplicate_boolean_literal_is_redundant() {
        let arms = vec![
            (Pattern::Lit(Literal::Bool(true)), false),
            (Pattern::Lit(Literal::Bool(true)), true),
            (Pattern::Lit(Literal::Bool(false)), false),
        ];
        let report = analyze_match(&Type::bool(), &arms).unwrap();
        assert!(report.is_exhaustive());
        assert_eq!(report.redundant_arms, vec![1]);
    }

    #[test]
    fn leaves_infinite_domains_to_runtime_fallback() {
        let arms = vec![(Pattern::Wild, true)];
        assert!(analyze_match(&Type::int(), &arms).is_none());
    }

    #[test]
    fn emits_non_exhaustive_warning_without_changing_validity() {
        let arms = vec![(variant("Red"), false), (variant("Green"), false)];
        let warnings = warnings_for_match(&color_type(), &arms, Span::new(10, 20));
        assert_eq!(warnings.len(), 1);
        assert_eq!(warnings[0].code, "W0201");
        assert!(warnings[0].msg.contains("Blue"));
    }

    #[test]
    fn emits_boolean_non_exhaustive_warning() {
        let arms = vec![(Pattern::Lit(Literal::Bool(true)), false)];
        let warnings = warnings_for_match(&Type::bool(), &arms, Span::default());
        assert_eq!(warnings.len(), 1);
        assert_eq!(warnings[0].code, "W0201");
        assert!(warnings[0].msg.contains("false"));
    }

    #[test]
    fn emits_redundant_arm_warning_with_one_based_arm_numbers() {
        let arms = vec![
            (variant("Red"), false),
            (variant("Red"), false),
            (Pattern::Wild, false),
            (variant("Blue"), false),
        ];
        let warnings = warnings_for_match(&color_type(), &arms, Span::default());
        assert_eq!(warnings.len(), 1);
        assert_eq!(warnings[0].code, "W0202");
        assert!(warnings[0].msg.contains("2, 4"));
    }

    #[test]
    fn can_emit_coverage_and_redundancy_warnings_together() {
        let arms = vec![
            (variant("Red"), false),
            (variant("Red"), false),
            (variant("Green"), false),
        ];
        let warnings = warnings_for_match(&color_type(), &arms, Span::default());
        assert_eq!(warnings.len(), 2);
        assert_eq!(warnings[0].code, "W0201");
        assert_eq!(warnings[1].code, "W0202");
    }
}
