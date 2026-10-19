//! Conservative compile-time diagnostics for finite-domain `match` expressions.
//!
//! This is deliberately a warning-only analysis: the Nulang 1.x compatibility
//! contract preserves runtime non-exhaustive behavior. Only irrefutable patterns
//! count toward proof of coverage. Unsupported domains produce no warnings.
use std::collections::BTreeSet;

use crate::ast::{Expr, Literal, Pattern};
use crate::types::{NuWarning, PrimitiveType, Span, Type};

enum FiniteDomain<'a> {
    Bool,
    Variant(&'a [(String, Option<Type>)]),
}

fn finite_domain(ty: &Type) -> Option<FiniteDomain<'_>> {
    match ty {
        Type::Primitive(PrimitiveType::Bool) => Some(FiniteDomain::Bool),
        Type::Variant(variants) if !variants.is_empty() => Some(FiniteDomain::Variant(variants)),
        _ => None,
    }
}

fn irrefutable(pattern: &Pattern) -> bool {
    match pattern {
        Pattern::Wild | Pattern::Var(_) => true,
        Pattern::Alias(_, inner) => irrefutable(inner),
        _ => false,
    }
}

/// Return one key for a *whole* finite-domain constructor/value matched by
/// an unguarded arm. Refutable payload patterns intentionally return None.
fn covered_key(pattern: &Pattern, domain: &FiniteDomain<'_>) -> Option<String> {
    if let Pattern::Alias(_, inner) = pattern {
        return covered_key(inner, domain);
    }
    match (pattern, domain) {
        (Pattern::Lit(Literal::Bool(b)), FiniteDomain::Bool) => Some(b.to_string()),
        (Pattern::Variant(name, payload_pattern), FiniteDomain::Variant(variants)) => {
            let (_, payload_type) = variants.iter().find(|(variant, _)| variant == name)?;
            let covers_payload = match (payload_type, payload_pattern) {
                (None, None) => true,
                (Some(_), Some(pattern)) => irrefutable(pattern),
                _ => false,
            };
            covers_payload.then(|| name.clone())
        }
        _ => None,
    }
}

/// Analyze only statically finite Bool/closed-variant scrutinees.
///
/// `W0201`: a missing case is provable.
/// `W0202`: an arm is unreachable because an earlier unguarded arm already
/// covers it. Patterns without source spans currently use the match span.
/// Guards are NEVER assumed to be total, even when they are constant true.
/// No warning here changes the validity of source programs by default.
pub fn check_coverage(
    scrutinee_type: &Type,
    arms: &[(Pattern, Option<Expr>, Expr)],
    span: Span,
) -> Vec<NuWarning> {
    let Some(domain) = finite_domain(scrutinee_type) else {
        return Vec::new();
    };
    let all: BTreeSet<String> = match &domain {
        FiniteDomain::Bool => ["false".to_string(), "true".to_string()].into_iter().collect(),
        FiniteDomain::Variant(variants) => variants.iter().map(|(name, _)| name.clone()).collect(),
    };
    let mut covered = BTreeSet::new();
    let mut warnings = Vec::new();
    let mut complete = false;

    for (pattern, guard, _) in arms {
        let key = covered_key(pattern, &domain);
        if complete || key.as_ref().is_some_and(|key| covered.contains(key)) {
            warnings.push(NuWarning {
                code: "W0202",
                msg: "unreachable match arm: already covered by an earlier unguarded arm"
                    .to_string(),
                span,
                help: Some("remove the redundant arm or adjust the preceding pattern".to_string()),
            });
        }
        if guard.is_none() {
            if irrefutable(pattern) {
                complete = true;
            } else if let Some(key) = key {
                covered.insert(key);
                complete = covered.len() == all.len();
            }
        }
    }
    if !complete {
        let missing: Vec<&str> = all
            .iter()
            .filter(|key| !covered.contains(*key))
            .map(String::as_str)
            .collect();
        if !missing.is_empty() {
            warnings.push(NuWarning {
                code: "W0201",
                msg: format!("non-exhaustive match: missing {}", missing.join(", ")),
                span,
                help: Some("add an unguarded case for each missing value or a wildcard arm"
                    .to_string()),
            });
        }
    }
    warnings
}
