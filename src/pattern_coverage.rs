//! Conservative static coverage analysis for `match` expressions.
//!
//! The runtime already retains a non-exhaustive-match fallback for cases the
//! compiler cannot prove. This module handles the decidable finite-variant
//! subset: declared variants, constructor arms, guards, and irrefutable
//! catch-alls. It deliberately prefers false negatives (leave the runtime
//! fallback in place) over false positives (claim an arm is exhaustive when it
//! is not).

use crate::ast::Pattern;
use crate::types::Type;
use std::collections::HashSet;

/// Result of statically analysing a finite match.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct CoverageReport {
    /// Human-readable witnesses for values not covered by any unguarded arm.
    pub missing: Vec<String>,
    /// Zero-based arm indexes that can never be reached because an earlier
    /// unguarded arm already covers the same constructor (or all values).
    pub redundant_arms: Vec<usize>,
}

impl CoverageReport {
    pub fn is_exhaustive(&self) -> bool {
        self.missing.is_empty()
    }
}

/// Analyse a match when its scrutinee has a statically finite variant type.
///
/// `arms` stores the pattern and whether that arm has a guard. Guarded arms do
/// not contribute to exhaustiveness because the guard may evaluate to false.
/// Returns `None` for non-variant scrutinees; those continue to rely on the
/// existing runtime non-exhaustive fallback.
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

        if is_irrefutable(pattern) {
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
        (Some(_), Some(inner)) if is_irrefutable(inner) => Some(name.as_str()),
        _ => None,
    }
}

fn strip_alias(mut pattern: &Pattern) -> &Pattern {
    while let Pattern::Alias(_, inner) = pattern {
        pattern = inner;
    }
    pattern
}

/// Whether the pattern accepts every value of its already-known input type.
///
/// Tuple and record patterns are included when all children are irrefutable;
/// their outer shape is guaranteed by the typechecker before this analysis is
/// consumed. Literals and variant constructors are refutable by definition.
fn is_irrefutable(pattern: &Pattern) -> bool {
    match pattern {
        Pattern::Wild | Pattern::Var(_) => true,
        Pattern::Alias(_, inner) => is_irrefutable(inner),
        Pattern::Tuple(items) => items.iter().all(is_irrefutable),
        Pattern::Record(fields) => fields.iter().all(|(_, pat)| is_irrefutable(pat)),
        Pattern::Lit(_) | Pattern::Variant(_, _) => false,
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
        use crate::ast::Literal;
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
    fn leaves_infinite_domains_to_runtime_fallback() {
        let arms = vec![(Pattern::Wild, true)];
        assert!(analyze_variant_match(&Type::int(), &arms).is_none());
    }
}
