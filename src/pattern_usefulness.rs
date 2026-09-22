//! Maranget-style pattern usefulness and exhaustiveness analysis.
//!
//! The checker operates on the typed pattern space rather than source syntax
//! alone. It supports finite algebraic constructors (variants, booleans,
//! tuples, records, Nil/Unit) and remains exact for literal patterns over
//! infinite primitive domains by using the standard default-matrix rule.
//! Guards intentionally do not contribute to coverage because they may fail.

use crate::ast::{Literal, Pattern};
use crate::types::{PrimitiveType, Type, RECORD_ROW_TAIL_FIELD};
use std::collections::HashSet;

const MAX_MISSING_WITNESSES: usize = 8;

#[derive(Debug, Clone, PartialEq)]
enum CorePat {
    Wild,
    Ctor(Constructor, Vec<CorePat>),
    /// A pattern whose shape cannot be reconciled with the inferred type.
    /// Treat it as matching nothing for usefulness purposes.
    Impossible,
}

#[derive(Debug, Clone, PartialEq, Eq, Hash)]
enum Constructor {
    Bool(bool),
    Nil,
    Unit,
    Int(i64),
    Float(u64),
    String(String),
    Variant(String),
    Tuple(usize),
    Record(Vec<String>),
    /// Keeps malformed/unsupported source patterns distinct without allowing
    /// them to count toward a type's complete constructor signature.
    Opaque(String),
}

#[derive(Debug, Clone)]
struct ConstructorInfo {
    ctor: Constructor,
    arg_types: Vec<Type>,
}

#[derive(Debug, Default, Clone, PartialEq, Eq)]
pub(crate) struct CoverageReport {
    /// Rendered witness patterns that are not covered by any unguarded arm.
    pub missing: Vec<String>,
    /// Zero-based arm indices whose pattern is not useful given previous
    /// unguarded arms.
    pub unreachable_arms: Vec<usize>,
}

type Matrix = Vec<Vec<CorePat>>;

/// Analyze one typed `match`.
///
/// `arms` contains each arm pattern and whether that arm has a guard.
/// Guarded arms are checked for reachability, but never added to the coverage
/// matrix because a false guard allows control to continue to later arms.
pub(crate) fn analyze_match(ty: &Type, arms: &[(&Pattern, bool)]) -> CoverageReport {
    let mut matrix: Matrix = Vec::new();
    let mut unreachable_arms = Vec::new();

    for (index, (pattern, guarded)) in arms.iter().enumerate() {
        let core = normalize_pattern(pattern, ty);
        let query = vec![core.clone()];
        if !is_useful(&matrix, &query, std::slice::from_ref(ty)) {
            unreachable_arms.push(index);
        }

        if !*guarded && !matches!(core, CorePat::Impossible) {
            matrix.push(vec![core]);
        }
    }

    let mut missing = Vec::new();
    let mut witness_matrix = matrix;
    let wildcard_query = vec![CorePat::Wild];

    for _ in 0..MAX_MISSING_WITNESSES {
        let Some(witness) =
            find_witness(&witness_matrix, &wildcard_query, std::slice::from_ref(ty))
        else {
            break;
        };
        let Some(first) = witness.first() else {
            break;
        };
        let rendered = render_pattern(first);
        if missing.contains(&rendered) {
            break;
        }
        missing.push(rendered);
        // Add the witness back into the matrix so subsequent iterations find a
        // distinct uncovered region. A witness may intentionally contain '_'
        // for an infinite remainder (e.g. Some(_)); in that case it compactly
        // represents the entire uncovered class.
        witness_matrix.push(witness);
    }

    CoverageReport {
        missing,
        unreachable_arms,
    }
}

fn normalize_pattern(pattern: &Pattern, ty: &Type) -> CorePat {
    let ty = peel_nominal(ty);
    match pattern {
        Pattern::Wild | Pattern::Var(_) => CorePat::Wild,
        Pattern::Alias(_, inner) => normalize_pattern(inner, ty),
        Pattern::Lit(lit) => normalize_literal(lit, ty),
        Pattern::Variant(name, payload) => match ty {
            Type::Variant(variants) => {
                let Some((_, expected_payload)) =
                    variants.iter().find(|(variant, _)| variant == name)
                else {
                    return CorePat::Ctor(
                        Constructor::Opaque(format!("variant:{name}")),
                        Vec::new(),
                    );
                };
                match (expected_payload, payload.as_deref()) {
                    (None, None) => CorePat::Ctor(Constructor::Variant(name.clone()), Vec::new()),
                    (Some(payload_ty), Some(inner)) => CorePat::Ctor(
                        Constructor::Variant(name.clone()),
                        vec![normalize_pattern(inner, payload_ty)],
                    ),
                    // A bare pattern for a payload-carrying constructor (or a
                    // payload pattern for a nullary constructor) does not have
                    // the runtime representation required to match it.
                    _ => CorePat::Impossible,
                }
            }
            _ => CorePat::Ctor(Constructor::Opaque(format!("variant:{name}")), Vec::new()),
        },
        Pattern::Tuple(items) => match ty {
            Type::Tuple(types) if items.len() == types.len() => CorePat::Ctor(
                Constructor::Tuple(types.len()),
                items
                    .iter()
                    .zip(types)
                    .map(|(pattern, ty)| normalize_pattern(pattern, ty))
                    .collect(),
            ),
            _ => CorePat::Ctor(Constructor::Opaque(format!("tuple:{items:?}")), Vec::new()),
        },
        Pattern::Record(pattern_fields) => match ty {
            Type::Record(type_fields) => {
                let real_fields: Vec<(&String, &Type)> = type_fields
                    .iter()
                    .filter(|(name, _)| name != RECORD_ROW_TAIL_FIELD)
                    .map(|(name, ty)| (name, ty))
                    .collect();

                if pattern_fields.iter().any(|(name, _)| {
                    !real_fields
                        .iter()
                        .any(|(field_name, _)| field_name.as_str() == name)
                }) {
                    return CorePat::Ctor(
                        Constructor::Opaque(format!("record:{pattern_fields:?}")),
                        Vec::new(),
                    );
                }

                let field_names = real_fields
                    .iter()
                    .map(|(name, _)| (*name).clone())
                    .collect::<Vec<_>>();
                let args = real_fields
                    .iter()
                    .map(|(name, ty)| {
                        pattern_fields
                            .iter()
                            .find(|(pattern_name, _)| pattern_name == *name)
                            .map(|(_, pattern)| normalize_pattern(pattern, ty))
                            .unwrap_or(CorePat::Wild)
                    })
                    .collect();
                CorePat::Ctor(Constructor::Record(field_names), args)
            }
            _ => CorePat::Ctor(
                Constructor::Opaque(format!("record:{pattern_fields:?}")),
                Vec::new(),
            ),
        },
    }
}

fn normalize_literal(lit: &Literal, ty: &Type) -> CorePat {
    let expected = peel_nominal(ty);
    let ctor = match lit {
        Literal::Int(value) if matches!(expected, Type::Primitive(PrimitiveType::Int)) => {
            Constructor::Int(*value)
        }
        Literal::Float(value) if matches!(expected, Type::Primitive(PrimitiveType::Float)) => {
            Constructor::Float(value.to_bits())
        }
        Literal::String(value) if matches!(expected, Type::Primitive(PrimitiveType::String)) => {
            Constructor::String(value.clone())
        }
        Literal::Bool(value) if matches!(expected, Type::Primitive(PrimitiveType::Bool)) => {
            Constructor::Bool(*value)
        }
        Literal::Nil if matches!(expected, Type::Primitive(PrimitiveType::Nil)) => Constructor::Nil,
        Literal::Unit if matches!(expected, Type::Primitive(PrimitiveType::Unit)) => {
            Constructor::Unit
        }
        // Type inference normally rejects these before coverage runs. Keep an
        // opaque constructor as a conservative fallback if a partially-known
        // type reaches this phase.
        _ => Constructor::Opaque(format!("literal:{lit:?}")),
    };
    CorePat::Ctor(ctor, Vec::new())
}

fn peel_nominal(mut ty: &Type) -> &Type {
    while let Type::Nominal { underlying, .. } = ty {
        ty = underlying;
    }
    ty
}

fn constructor_universe(ty: &Type) -> Option<Vec<ConstructorInfo>> {
    match peel_nominal(ty) {
        Type::Primitive(PrimitiveType::Bool) => Some(vec![
            ConstructorInfo {
                ctor: Constructor::Bool(true),
                arg_types: Vec::new(),
            },
            ConstructorInfo {
                ctor: Constructor::Bool(false),
                arg_types: Vec::new(),
            },
        ]),
        Type::Primitive(PrimitiveType::Nil) => Some(vec![ConstructorInfo {
            ctor: Constructor::Nil,
            arg_types: Vec::new(),
        }]),
        Type::Primitive(PrimitiveType::Unit) => Some(vec![ConstructorInfo {
            ctor: Constructor::Unit,
            arg_types: Vec::new(),
        }]),
        Type::Primitive(PrimitiveType::Never) => Some(Vec::new()),
        Type::Variant(variants) => Some(
            variants
                .iter()
                .map(|(name, payload)| ConstructorInfo {
                    ctor: Constructor::Variant(name.clone()),
                    arg_types: payload.iter().cloned().collect(),
                })
                .collect(),
        ),
        Type::Tuple(types) => Some(vec![ConstructorInfo {
            ctor: Constructor::Tuple(types.len()),
            arg_types: types.clone(),
        }]),
        Type::Record(fields) => {
            let real_fields = fields
                .iter()
                .filter(|(name, _)| name != RECORD_ROW_TAIL_FIELD)
                .collect::<Vec<_>>();
            Some(vec![ConstructorInfo {
                ctor: Constructor::Record(
                    real_fields.iter().map(|(name, _)| name.clone()).collect(),
                ),
                arg_types: real_fields.iter().map(|(_, ty)| (*ty).clone()).collect(),
            }])
        }
        // Int/Float/String and the remaining runtime domains have an open or
        // infinite constructor set. The default matrix handles them exactly
        // for literal-vs-wildcard usefulness without enumerating the domain.
        _ => None,
    }
}

fn constructor_arg_types(ty: &Type, ctor: &Constructor) -> Vec<Type> {
    let Some(universe) = constructor_universe(ty) else {
        return Vec::new();
    };
    universe
        .into_iter()
        .find(|info| &info.ctor == ctor)
        .map(|info| info.arg_types)
        .unwrap_or_default()
}

fn column_constructors(matrix: &Matrix) -> HashSet<Constructor> {
    matrix
        .iter()
        .filter_map(|row| match row.first() {
            Some(CorePat::Ctor(ctor, _)) => Some(ctor.clone()),
            _ => None,
        })
        .collect()
}

fn specialize(matrix: &Matrix, ctor: &Constructor, arity: usize) -> Matrix {
    let mut out = Vec::new();
    for row in matrix {
        let Some(head) = row.first() else {
            continue;
        };
        match head {
            CorePat::Wild => {
                let mut new_row = vec![CorePat::Wild; arity];
                new_row.extend_from_slice(&row[1..]);
                out.push(new_row);
            }
            CorePat::Ctor(row_ctor, args) if row_ctor == ctor && args.len() == arity => {
                let mut new_row = args.clone();
                new_row.extend_from_slice(&row[1..]);
                out.push(new_row);
            }
            CorePat::Impossible | CorePat::Ctor(_, _) => {}
        }
    }
    out
}

fn default_matrix(matrix: &Matrix) -> Matrix {
    matrix
        .iter()
        .filter_map(|row| match row.first() {
            Some(CorePat::Wild) => Some(row[1..].to_vec()),
            _ => None,
        })
        .collect()
}

fn is_useful(matrix: &Matrix, query: &[CorePat], types: &[Type]) -> bool {
    if query.is_empty() {
        return matrix.is_empty();
    }
    let Some(query_head) = query.first() else {
        return matrix.is_empty();
    };
    let Some(ty_head) = types.first() else {
        return false;
    };

    match query_head {
        CorePat::Impossible => false,
        CorePat::Ctor(ctor, args) => {
            let mut next_query = args.clone();
            next_query.extend_from_slice(&query[1..]);

            let mut next_types = constructor_arg_types(ty_head, ctor);
            next_types.extend_from_slice(&types[1..]);

            is_useful(
                &specialize(matrix, ctor, args.len()),
                &next_query,
                &next_types,
            )
        }
        CorePat::Wild => {
            if let Some(universe) = constructor_universe(ty_head) {
                let present = column_constructors(matrix);
                let complete = universe.iter().all(|info| present.contains(&info.ctor));
                if complete {
                    if universe.is_empty() {
                        return false;
                    }
                    return universe.into_iter().any(|info| {
                        let mut next_query = vec![CorePat::Wild; info.arg_types.len()];
                        next_query.extend_from_slice(&query[1..]);

                        let mut next_types = info.arg_types.clone();
                        next_types.extend_from_slice(&types[1..]);

                        is_useful(
                            &specialize(matrix, &info.ctor, info.arg_types.len()),
                            &next_query,
                            &next_types,
                        )
                    });
                }
            }

            is_useful(&default_matrix(matrix), &query[1..], &types[1..])
        }
    }
}

fn find_witness(matrix: &Matrix, query: &[CorePat], types: &[Type]) -> Option<Vec<CorePat>> {
    if query.is_empty() {
        return matrix.is_empty().then(Vec::new);
    }
    let query_head = query.first()?;
    let ty_head = types.first()?;

    match query_head {
        CorePat::Impossible => None,
        CorePat::Ctor(ctor, args) => {
            let mut next_query = args.clone();
            next_query.extend_from_slice(&query[1..]);

            let arg_types = constructor_arg_types(ty_head, ctor);
            let mut next_types = arg_types;
            next_types.extend_from_slice(&types[1..]);

            let witness = find_witness(
                &specialize(matrix, ctor, args.len()),
                &next_query,
                &next_types,
            )?;
            let (ctor_args, tail) = witness.split_at(args.len());
            let mut rebuilt = vec![CorePat::Ctor(ctor.clone(), ctor_args.to_vec())];
            rebuilt.extend_from_slice(tail);
            Some(rebuilt)
        }
        CorePat::Wild => {
            if let Some(universe) = constructor_universe(ty_head) {
                let present = column_constructors(matrix);
                let complete = universe.iter().all(|info| present.contains(&info.ctor));

                if complete {
                    for info in universe {
                        let arity = info.arg_types.len();
                        let mut next_query = vec![CorePat::Wild; arity];
                        next_query.extend_from_slice(&query[1..]);

                        let mut next_types = info.arg_types.clone();
                        next_types.extend_from_slice(&types[1..]);

                        if let Some(witness) = find_witness(
                            &specialize(matrix, &info.ctor, arity),
                            &next_query,
                            &next_types,
                        ) {
                            let (ctor_args, tail) = witness.split_at(arity);
                            let mut rebuilt =
                                vec![CorePat::Ctor(info.ctor.clone(), ctor_args.to_vec())];
                            rebuilt.extend_from_slice(tail);
                            return Some(rebuilt);
                        }
                    }
                    return None;
                }

                if let Some(tail) = find_witness(&default_matrix(matrix), &query[1..], &types[1..])
                {
                    if let Some(info) = universe
                        .into_iter()
                        .find(|info| !present.contains(&info.ctor))
                    {
                        let mut rebuilt = vec![CorePat::Ctor(
                            info.ctor,
                            vec![CorePat::Wild; info.arg_types.len()],
                        )];
                        rebuilt.extend_from_slice(&tail);
                        return Some(rebuilt);
                    }
                }
                return None;
            }

            let tail = find_witness(&default_matrix(matrix), &query[1..], &types[1..])?;
            let mut rebuilt = vec![CorePat::Wild];
            rebuilt.extend_from_slice(&tail);
            Some(rebuilt)
        }
    }
}

fn render_pattern(pattern: &CorePat) -> String {
    match pattern {
        CorePat::Wild => "_".to_string(),
        CorePat::Impossible => "<impossible>".to_string(),
        CorePat::Ctor(ctor, args) => match ctor {
            Constructor::Bool(value) => value.to_string(),
            Constructor::Nil => "nil".to_string(),
            Constructor::Unit => "()".to_string(),
            Constructor::Int(value) => value.to_string(),
            Constructor::Float(bits) => f64::from_bits(*bits).to_string(),
            Constructor::String(value) => format!("{value:?}"),
            Constructor::Variant(name) => {
                if let Some(payload) = args.first() {
                    format!("{name}({})", render_pattern(payload))
                } else {
                    name.clone()
                }
            }
            Constructor::Tuple(_) => format!(
                "({})",
                args.iter()
                    .map(render_pattern)
                    .collect::<Vec<_>>()
                    .join(", ")
            ),
            Constructor::Record(names) => format!(
                "{{ {} }}",
                names
                    .iter()
                    .zip(args)
                    .map(|(name, pattern)| format!("{name}: {}", render_pattern(pattern)))
                    .collect::<Vec<_>>()
                    .join(", ")
            ),
            Constructor::Opaque(label) => format!("<{label}>"),
        },
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn lit_bool(value: bool) -> Pattern {
        Pattern::Lit(Literal::Bool(value))
    }

    fn lit_int(value: i64) -> Pattern {
        Pattern::Lit(Literal::Int(value))
    }

    #[test]
    fn nested_variant_matrix_is_exhaustive() {
        let ty = Type::Variant(vec![
            ("Some".to_string(), Some(Type::bool())),
            ("None".to_string(), None),
        ]);
        let patterns = vec![
            Pattern::Variant("Some".to_string(), Some(Box::new(lit_bool(true)))),
            Pattern::Variant("Some".to_string(), Some(Box::new(lit_bool(false)))),
            Pattern::Variant("None".to_string(), None),
        ];
        let arms = patterns
            .iter()
            .map(|pattern| (pattern, false))
            .collect::<Vec<_>>();
        let report = analyze_match(&ty, &arms);
        assert!(report.missing.is_empty());
        assert!(report.unreachable_arms.is_empty());
    }

    #[test]
    fn nested_variant_reports_precise_missing_witness() {
        let ty = Type::Variant(vec![
            ("Some".to_string(), Some(Type::bool())),
            ("None".to_string(), None),
        ]);
        let patterns = vec![
            Pattern::Variant("Some".to_string(), Some(Box::new(lit_bool(true)))),
            Pattern::Variant("None".to_string(), None),
        ];
        let arms = patterns
            .iter()
            .map(|pattern| (pattern, false))
            .collect::<Vec<_>>();
        let report = analyze_match(&ty, &arms);
        assert_eq!(report.missing, vec!["Some(false)"]);
    }

    #[test]
    fn duplicate_literal_on_infinite_domain_is_unreachable() {
        let patterns = [lit_int(1), lit_int(1)];
        let arms = patterns
            .iter()
            .map(|pattern| (pattern, false))
            .collect::<Vec<_>>();
        let report = analyze_match(&Type::int(), &arms);
        assert_eq!(report.unreachable_arms, vec![1]);
        assert_eq!(report.missing, vec!["_"]);
    }

    #[test]
    fn finite_complete_match_makes_fallback_unreachable() {
        let patterns = [lit_bool(true), lit_bool(false), Pattern::Wild];
        let arms = patterns
            .iter()
            .map(|pattern| (pattern, false))
            .collect::<Vec<_>>();
        let report = analyze_match(&Type::bool(), &arms);
        assert!(report.missing.is_empty());
        assert_eq!(report.unreachable_arms, vec![2]);
    }

    #[test]
    fn guarded_wildcard_does_not_shadow_later_arms() {
        let patterns = [Pattern::Wild, lit_bool(true), lit_bool(false)];
        let arms = vec![
            (&patterns[0], true),
            (&patterns[1], false),
            (&patterns[2], false),
        ];
        let report = analyze_match(&Type::bool(), &arms);
        assert!(report.missing.is_empty());
        assert!(report.unreachable_arms.is_empty());
    }

    #[test]
    fn tuple_matrix_tracks_nested_product_space() {
        let ty = Type::Tuple(vec![Type::bool(), Type::bool()]);
        let patterns = vec![
            Pattern::Tuple(vec![lit_bool(true), Pattern::Wild]),
            Pattern::Tuple(vec![lit_bool(false), lit_bool(true)]),
            Pattern::Tuple(vec![lit_bool(false), lit_bool(false)]),
        ];
        let arms = patterns
            .iter()
            .map(|pattern| (pattern, false))
            .collect::<Vec<_>>();
        let report = analyze_match(&ty, &arms);
        assert!(report.missing.is_empty());
        assert!(report.unreachable_arms.is_empty());
    }

    #[test]
    fn tuple_subsumption_marks_later_arm_unreachable() {
        let ty = Type::Tuple(vec![Type::bool(), Type::bool()]);
        let patterns = vec![
            Pattern::Tuple(vec![lit_bool(true), Pattern::Wild]),
            Pattern::Tuple(vec![lit_bool(true), lit_bool(false)]),
        ];
        let arms = patterns
            .iter()
            .map(|pattern| (pattern, false))
            .collect::<Vec<_>>();
        let report = analyze_match(&ty, &arms);
        assert_eq!(report.unreachable_arms, vec![1]);
        assert!(report.missing.contains(&"(false, true)".to_string()));
        assert!(report.missing.contains(&"(false, false)".to_string()));
    }

    #[test]
    fn record_product_space_is_analyzed_structurally() {
        let ty = Type::Record(vec![
            ("a".to_string(), Type::bool()),
            ("b".to_string(), Type::bool()),
        ]);
        let patterns = vec![
            Pattern::Record(vec![("a".to_string(), lit_bool(true))]),
            Pattern::Record(vec![
                ("a".to_string(), lit_bool(false)),
                ("b".to_string(), lit_bool(true)),
            ]),
            Pattern::Record(vec![
                ("a".to_string(), lit_bool(false)),
                ("b".to_string(), lit_bool(false)),
            ]),
        ];
        let arms = patterns
            .iter()
            .map(|pattern| (pattern, false))
            .collect::<Vec<_>>();
        let report = analyze_match(&ty, &arms);
        assert!(report.missing.is_empty());
        assert!(report.unreachable_arms.is_empty());
    }
}
