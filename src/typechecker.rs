//! Hindley-Milner type checker (Algorithm W) for Nulang.
//!
//! Implements classical Damas-Milner type inference with support for:
//! - Primitive types (Int, Float, Bool, String, Unit, Never, Address)
//! - Polymorphism via type schemes (forall vars. Type)
//! - Tuples, Records, Variants, Arrays
//! - Functions with effect rows and capability annotations
//! - Reference types with capabilities
//! - Actor types
//! - Pattern matching
//! - Binary and unary operators
//!
//! The algorithm follows the standard substitution-based approach:
//! 1. `infer` computes a type and a substitution
//! 2. `mgu` (most general unifier) produces substitutions from equality constraints
//! 3. `apply_subst` propagates substitutions through types
//! 4. `generalize` creates polymorphic schemes from free variables
//! 5. `instantiate` creates fresh type variables from schemes

use crate::ast::*;
use crate::types::*;
use std::collections::HashSet;

// ---------------------------------------------------------------------------
// Substitution
// ---------------------------------------------------------------------------

/// A substitution maps type variables to types.
/// Ordered list: earlier substitutions take precedence.
pub type Substitution = Vec<(TypeVar, Type)>;
// Fast hashing for compiler-internal maps (keys are not attacker-controlled).
type FxHashMap<K, V> =
    std::collections::HashMap<K, V, std::hash::BuildHasherDefault<rustc_hash::FxHasher>>;
type FxHashSet<T> =
    std::collections::HashSet<T, std::hash::BuildHasherDefault<rustc_hash::FxHasher>>;

/// Apply a substitution to a type, replacing any type variables that appear
/// in the substitution with their mapped types.
pub(crate) fn apply_subst(ty: &Type, subst: &Substitution) -> Type {
    match ty {
        Type::Var(v) => {
            // Find the first mapping for this variable
            for (var, replacement) in subst {
                if var == v {
                    // Apply recursively in case the replacement contains vars
                    // that are also in the substitution
                    return apply_subst(replacement, subst);
                }
            }
            Type::Var(*v)
        }
        Type::Primitive(_) => ty.clone(),
        Type::Skolem(_) => ty.clone(),
        Type::Tuple(ts) => Type::Tuple(ts.iter().map(|t| apply_subst(t, subst)).collect()),
        Type::Record(fs) => Type::Record(
            fs.iter()
                .map(|(n, t)| (n.clone(), apply_subst(t, subst)))
                .collect(),
        ),
        Type::Variant(vs) => Type::Variant(
            vs.iter()
                .map(|(name, t)| (name.clone(), t.as_ref().map(|t| apply_subst(t, subst))))
                .collect(),
        ),
        Type::Array(t) => Type::Array(Box::new(apply_subst(t, subst))),
        Type::Function {
            param,
            ret,
            effect,
            cap,
        } => Type::Function {
            param: Box::new(apply_subst(param, subst)),
            ret: Box::new(apply_subst(ret, subst)),
            effect: effect.clone(),
            cap: *cap,
        },
        Type::Actor { state, behavior } => Type::Actor {
            state: Box::new(apply_subst(state, subst)),
            behavior: Box::new(apply_subst(behavior, subst)),
        },
        Type::App { constructor, args } => Type::App {
            constructor: Box::new(apply_subst(constructor, subst)),
            args: args.iter().map(|a| apply_subst(a, subst)).collect(),
        },
        Type::Reference { cap, inner } => Type::Reference {
            cap: *cap,
            inner: Box::new(apply_subst(inner, subst)),
        },
        Type::Scheme { vars, body } => {
            // Remove substitutions for bound variables
            let filtered: Substitution = subst
                .iter()
                .filter(|(v, _)| !vars.contains(v))
                .cloned()
                .collect();
            Type::Scheme {
                vars: vars.clone(),
                body: Box::new(apply_subst(body, &filtered)),
            }
        }
        Type::Nominal { name, underlying } => Type::Nominal {
            name: name.clone(),
            underlying: Box::new(apply_subst(underlying, subst)),
        },
    }
}

/// Substitute specific type variables with given types. Used for
/// skolemization: replace each type-parameter variable with a Skolem.
fn subst_type_vars_with(ty: &Type, map: &FxHashMap<TypeVar, Type>) -> Type {
    match ty {
        Type::Var(v) => map.get(v).cloned().unwrap_or_else(|| ty.clone()),
        Type::Primitive(_) => ty.clone(),
        Type::Tuple(ts) => Type::Tuple(ts.iter().map(|t| subst_type_vars_with(t, map)).collect()),
        Type::Record(fs) => Type::Record(
            fs.iter()
                .map(|(n, t)| (n.clone(), subst_type_vars_with(t, map)))
                .collect(),
        ),
        Type::Variant(vs) => Type::Variant(
            vs.iter()
                .map(|(n, t)| (n.clone(), t.as_ref().map(|t| subst_type_vars_with(t, map))))
                .collect(),
        ),
        Type::Array(t) => Type::Array(Box::new(subst_type_vars_with(t, map))),
        Type::Function {
            param,
            ret,
            effect,
            cap,
        } => Type::Function {
            param: Box::new(subst_type_vars_with(param, map)),
            ret: Box::new(subst_type_vars_with(ret, map)),
            effect: effect.clone(),
            cap: *cap,
        },
        Type::Actor { state, behavior } => Type::Actor {
            state: Box::new(subst_type_vars_with(state, map)),
            behavior: Box::new(subst_type_vars_with(behavior, map)),
        },
        Type::App { constructor, args } => Type::App {
            constructor: Box::new(subst_type_vars_with(constructor, map)),
            args: args.iter().map(|a| subst_type_vars_with(a, map)).collect(),
        },
        Type::Reference { cap, inner } => Type::Reference {
            cap: *cap,
            inner: Box::new(subst_type_vars_with(inner, map)),
        },
        Type::Scheme { vars, body } => Type::Scheme {
            vars: vars.clone(),
            body: Box::new(subst_type_vars_with(body, map)),
        },
        Type::Nominal { name, underlying } => Type::Nominal {
            name: name.clone(),
            underlying: Box::new(subst_type_vars_with(underlying, map)),
        },
        Type::Skolem(_) => ty.clone(),
    }
}

/// Apply a substitution to a type context, returning the updated context.
/// Every binding's type is substituted so constraints inferred from earlier
/// subexpressions are visible at later uses of the same variable.
fn apply_subst_to_ctx(ctx: &TypeContext, subst: &Substitution) -> TypeContext {
    if subst.is_empty() {
        return ctx.clone();
    }
    let mut result = TypeContext::new();
    result.entity_events = ctx.entity_events.clone();
    for (name, (ty, cap, mutable)) in ctx.iter() {
        result.bind(name.clone(), apply_subst(ty, subst), *cap, *mutable);
    }
    // Propagate constraints through substitution: if a constrained type
    // variable is substituted to another variable, transfer the constraints.
    for (tv, class_names) in &ctx.constraints {
        let resolved = apply_subst(&Type::Var(*tv), subst);
        match resolved {
            Type::Var(tv2) => {
                for cn in class_names {
                    result.add_constraint(tv2, cn);
                }
            }
            // If substituted to a concrete type, drop the constraint — it
            // is resolved by the instance-lookup in B.4 when the caller
            // checks for a matching instance.
            _ => {}
        }
    }
    result
}

/// Strip the first (self) parameter from a function parameter type.
/// `fn(Self, A) -> Ret` becomes `fn(A) -> Ret`.
/// `fn(Self, A, B) -> Ret` becomes `fn((A, B)) -> Ret`.
/// `fn(Self) -> Ret` becomes `fn(Unit) -> Ret` (nullary after self removal).
fn strip_first_param(param: &Type) -> Type {
    match param {
        Type::Tuple(params) if params.len() > 2 => Type::Tuple(params[1..].to_vec()),
        Type::Tuple(params) if params.len() == 2 => {
            params[1].clone() // single remaining param, unwrapped
        }
        Type::Tuple(_) => Type::unit(), // only self, nullary
        _ => Type::unit(),
    }
}

/// Compose two substitutions: s2 after s1.
/// Result: first apply s1, then apply s2 to the result.
/// Formally: (s2 ∘ s1)(t) = s2(s1(t))
///
/// If a variable is bound by both substitutions, the two mappings are unified
/// and the unifier is composed through the result, so constraints from both
/// sides propagate (e.g. `a := (b, c)` from s1 and `a := (Int, Bool)` from s2
/// yields `b := Int, c := Bool`). Previously s2's mapping was silently
/// discarded, losing those constraints.
fn compose_subst(s2: &Substitution, s1: &Substitution) -> Substitution {
    // Apply s2 to all types in s1
    let mut s1_substituted: Substitution =
        s1.iter().map(|(v, t)| (*v, apply_subst(t, s2))).collect();
    for (v, t) in s2 {
        match s1_substituted.iter().position(|(rv, _)| rv == v) {
            // New binding from s2: keep it.
            None => s1_substituted.push((*v, t.clone())),
            // v is bound by both: unify the two mappings so neither
            // constraint is lost.
            Some(pos) => {
                let existing = s1_substituted[pos].1.clone();
                if let Ok(s) = mgu(&existing, t, Span::default()) {
                    let unified = apply_subst(&existing, &s);
                    s1_substituted.remove(pos);
                    s1_substituted.push((*v, unified));
                    s1_substituted = compose_subst(&s, &s1_substituted);
                }
                // Irreconcilable mappings cannot occur when contexts are
                // substituted eagerly (`apply_subst_to_ctx`): the conflicting
                // unification fails earlier, at the use site. Keep s1's
                // mapping rather than inventing a type.
            }
        }
    }
    s1_substituted
}

// ---------------------------------------------------------------------------
// Unification (Most General Unifier)
// ---------------------------------------------------------------------------

/// Check if two effect rows are compatible (can be unified).
/// For closed rows, they must have exactly the same effects.
/// For open rows, we check that the fixed effects are compatible.
fn effect_row_compatible(e1: &EffectRow, e2: &EffectRow) -> bool {
    match (e1, e2) {
        (EffectRow::Closed(a), EffectRow::Closed(b)) => {
            let mut a_sorted = a.clone();
            let mut b_sorted = b.clone();
            a_sorted.sort();
            b_sorted.sort();
            a_sorted == b_sorted
        }
        (EffectRow::Open(a, _), EffectRow::Closed(b))
        | (EffectRow::Closed(b), EffectRow::Open(a, _)) => b.iter().all(|e| a.contains(e)),
        (EffectRow::Open(a, _), EffectRow::Open(b, _)) => {
            // Both sides must agree on fixed effects; row variables are
            // assumed compatible (full row unification requires Region
            // to participate in the Type::Var substitution machinery,
            // which is a larger refactor — see REVIEW plan Phase 1 item 3).
            a.iter().all(|e| b.contains(e)) && b.iter().all(|e| a.contains(e))
        }
    }
}

/// Compute the most general unifier of two types.
/// Returns a substitution `s` such that `apply_subst(t1, s) == apply_subst(t2, s)`.
fn mgu(t1: &Type, t2: &Type, span: Span) -> NuResult<Substitution> {
    // Never is a subtype of everything — unify trivially.
    if matches!(t1, Type::Primitive(PrimitiveType::Never))
        || matches!(t2, Type::Primitive(PrimitiveType::Never))
    {
        return Ok(vec![]);
    }
    if t1 == t2 {
        return Ok(vec![]);
    }
    // NTIR erases opaque nominal wrappers to their underlying type, so two
    // ground types that differ only by opacity must not short-circuit here.
    // Let the Type::Nominal cases below enforce the opacity contract.
    if !matches!(t1, Type::Nominal { .. })
        && !matches!(t2, Type::Nominal { .. })
        && t1.is_ground()
        && t2.is_ground()
        && t1.to_ntir().hash() == t2.to_ntir().hash()
    {
        return Ok(vec![]);
    }

    match (t1, t2) {
        // Identical primitives unify trivially
        (Type::Primitive(a), Type::Primitive(b)) if a == b => Ok(vec![]),

        // Skolem types unify only with the same skolem
        (Type::Skolem(a), Type::Skolem(b)) if a == b => Ok(vec![]),

        // Type variable unification
        (Type::Var(v), t) | (t, Type::Var(v)) => var_subst(*v, t, span),

        // Functions: unify parameters, returns, effects, and capabilities
        (
            Type::Function {
                param: p1,
                ret: r1,
                effect: e1,
                cap: c1,
            },
            Type::Function {
                param: p2,
                ret: r2,
                effect: e2,
                cap: c2,
            },
        ) => {
            if c1 != c2 {
                return Err(NuError::type_mismatch(
                    format!("function with capability {}", c1),
                    format!("function with capability {}", c2),
                    span,
                ));
            }
            // Check effect row compatibility
            if !effect_row_compatible(e1, e2) {
                return Err(NuError::type_mismatch(
                    format!("function with effects {}", e1),
                    format!("function with effects {}", e2),
                    span,
                ));
            }
            let s1 = mgu(p1, p2, span)?;
            let s2 = mgu(&apply_subst(r1, &s1), &apply_subst(r2, &s1), span)?;
            Ok(compose_subst(&s2, &s1))
        }

        // Opaque nominal types: `opaque type X = Y` is distinct from Y.
        // Two nominal types unify only when they have the same name; their
        // underlying types are then unified to catch parameter mismatches.
        // A nominal type never unifies with its underlying type, a primitive,
        // or any other shape directly — explicit conversion functions must
        // bridge. (Future: allow transparent unification inside the defining
        // module once TypeChecker tracks per-module scope.)
        (
            Type::Nominal {
                name: n1,
                underlying: u1,
            },
            Type::Nominal {
                name: n2,
                underlying: u2,
            },
        ) => {
            if n1 == n2 {
                mgu(u1, u2, span)
            } else {
                Err(NuError::type_mismatch(
                    format!("opaque type {}", n1),
                    format!("opaque type {}", n2),
                    span,
                ))
            }
        }
        (Type::Nominal { name, .. }, other) | (other, Type::Nominal { name, .. }) => Err(
            NuError::type_mismatch(format!("opaque type {}", name), format!("{}", other), span),
        ),

        // Tuples
        (Type::Tuple(ts1), Type::Tuple(ts2)) => {
            if ts1.len() != ts2.len() {
                return Err(NuError::type_mismatch(
                    format!("tuple of {} elements", ts1.len()),
                    format!("tuple of {} elements", ts2.len()),
                    span,
                ));
            }
            unify_many(ts1, ts2, span)
        }

        // Records. Closed records (from literals and annotations) unify
        // exactly: identical field sets, pairwise field unification. Records
        // with an open row tail (produced by field access on a record of
        // not-yet-known shape) unify with scoped rows: shared fields unify
        // pairwise and the row variables absorb each other's extra fields; a
        // closed record unified with an open one must provide all of the open
        // record's fields and closes the row.
        (Type::Record(fs1), Type::Record(fs2)) => {
            let (fields1, tail1) = split_record(fs1);
            let (fields2, tail2) = split_record(fs2);
            match (&tail1, &tail2) {
                (None, None) => unify_closed_records(&fields1, &fields2, span),
                _ => unify_open_records(&fields1, &tail1, &fields2, &tail2, span),
            }
        }

        // Arrays
        (Type::Array(t1_inner), Type::Array(t2_inner)) => mgu(t1_inner, t2_inner, span),

        // Actors
        (
            Type::Actor {
                state: s1,
                behavior: b1,
            },
            Type::Actor {
                state: s2,
                behavior: b2,
            },
        ) => {
            let s_state = mgu(s1, s2, span)?;
            let s_beh = mgu(&apply_subst(b1, &s_state), &apply_subst(b2, &s_state), span)?;
            Ok(compose_subst(&s_beh, &s_state))
        }

        // Reference types
        (Type::Reference { cap: c1, inner: i1 }, Type::Reference { cap: c2, inner: i2 }) => {
            if c1 != c2 {
                return Err(NuError::type_mismatch(
                    format!("reference with capability {}", c1),
                    format!("reference with capability {}", c2),
                    span,
                ));
            }
            mgu(i1, i2, span)
        }

        // Generic type application
        (
            Type::App {
                constructor: c1,
                args: a1,
            },
            Type::App {
                constructor: c2,
                args: a2,
            },
        ) => {
            let s1 = mgu(c1, c2, span)?;
            let applied1: Vec<Type> = a1.iter().map(|t| apply_subst(t, &s1)).collect();
            let applied2: Vec<Type> = a2.iter().map(|t| apply_subst(t, &s1)).collect();
            let s2 = unify_many_app(&applied1, &applied2, span)?;
            Ok(compose_subst(&s2, &s1))
        }

        // Recursive type reference: a variant unified with a self-
        // referencing type variable applied to args (e.g. Tree[Int]).
        // The variant contains `App(Var(v), inner_args)` for each
        // recursive reference; unify inner_args with args.
        (Type::Variant(vs), Type::App { constructor, args })
        | (Type::App { constructor, args }, Type::Variant(vs))
            if matches!(constructor.as_ref(), Type::Var(_)) =>
        {
            let Type::Var(v) = constructor.as_ref() else {
                unreachable!()
            };
            let mut subst = vec![];
            let mut inner = Vec::new();
            find_recursive_apps(vs, *v, &mut inner);
            for inner_args in &inner {
                let s = unify_many_app(inner_args, args, span)?;
                subst = compose_subst(&s, &subst);
            }
            Ok(subst)
        }

        // Variants: same constructor set, payloads unify pairwise. Required
        // for declared variant types (SPEC2 §3.4.1), e.g. unifying the two
        // branches of `if b then Some(1) else None`.
        (Type::Variant(vs1), Type::Variant(vs2)) => {
            if vs1.len() != vs2.len() {
                return Err(NuError::type_mismatch(
                    format!("variant type with {} constructors", vs1.len()),
                    format!("variant type with {} constructors", vs2.len()),
                    span,
                ));
            }
            // Sort by constructor name so declaration order does not matter.
            let mut sorted1 = vs1.clone();
            let mut sorted2 = vs2.clone();
            sorted1.sort_by(|(a, _), (b, _)| a.cmp(b));
            sorted2.sort_by(|(a, _), (b, _)| a.cmp(b));
            let mut subst = vec![];
            for ((n1, p1), (n2, p2)) in sorted1.iter().zip(sorted2.iter()) {
                if n1 != n2 {
                    return Err(NuError::type_mismatch(
                        format!("constructor '{}'", n1),
                        format!("constructor '{}'", n2),
                        span,
                    ));
                }
                let s = match (p1, p2) {
                    (None, None) => continue,
                    (Some(a), Some(b)) => {
                        mgu(&apply_subst(a, &subst), &apply_subst(b, &subst), span)?
                    }
                    _ => {
                        return Err(NuError::type_mismatch(
                            format!("constructor '{}' with payload", n1),
                            format!("constructor '{}' without payload", n1),
                            span,
                        ));
                    }
                };
                subst = compose_subst(&s, &subst);
            }
            Ok(subst)
        }

        // Skolem types don't unify with anything else
        (Type::Skolem(_), _) | (_, Type::Skolem(_)) => Err(NuError::type_mismatch(
            format!("{}", t1),
            format!("{}", t2),
            span,
        )),

        // Anything else is a unification error
        _ => Err(NuError::type_mismatch(
            format!("{}", t1),
            format!("{}", t2),
            span,
        )),
    }
}

/// Split a record type's field list into its real fields and its optional
/// row tail, flattening nested record tails (`rho := { y: b | rho2 }`)
/// produced by row unification. A `None` tail means the record is closed.
/// See [`RECORD_ROW_TAIL_FIELD`] for the encoding.
fn split_record(fs: &[(String, Type)]) -> (Vec<(String, Type)>, Option<Type>) {
    let mut fields: Vec<(String, Type)> = Vec::new();
    let mut current: Vec<(String, Type)> = fs.to_vec();
    let tail = loop {
        let mut next_tail: Option<Type> = None;
        let mut rest: Vec<(String, Type)> = Vec::new();
        for (name, ty) in current {
            if name == RECORD_ROW_TAIL_FIELD {
                next_tail = Some(ty);
            } else {
                rest.push((name, ty));
            }
        }
        fields.extend(rest);
        match next_tail {
            Some(Type::Record(inner)) => current = inner,
            other => break other,
        }
    };
    (fields, tail)
}

/// Unify two closed records: identical field sets, pairwise field
/// unification. This is the exact pre-row-polymorphism behavior.
fn unify_closed_records(
    fs1: &[(String, Type)],
    fs2: &[(String, Type)],
    span: Span,
) -> NuResult<Substitution> {
    if fs1.len() != fs2.len() {
        return Err(NuError::type_mismatch(
            format!("record with {} fields", fs1.len()),
            format!("record with {} fields", fs2.len()),
            span,
        ));
    }
    // Sort by field name and unify corresponding fields
    let mut sorted1 = fs1.to_vec();
    let mut sorted2 = fs2.to_vec();
    sorted1.sort_by(|(a, _), (b, _)| a.cmp(b));
    sorted2.sort_by(|(a, _), (b, _)| a.cmp(b));
    let mut subst = vec![];
    for ((n1, t1f), (n2, t2f)) in sorted1.iter().zip(sorted2.iter()) {
        if n1 != n2 {
            return Err(NuError::type_mismatch(
                format!("record with field '{}'", n1),
                format!("record with field '{}'", n2),
                span,
            ));
        }
        let s = mgu(&apply_subst(t1f, &subst), &apply_subst(t2f, &subst), span)?;
        subst = compose_subst(&s, &subst);
    }
    Ok(subst)
}

/// Unify two record types where at least one side has an open row tail
/// (standard scoped-rows unification). `fields*` are the real fields and
/// `tail*` the optional row tail as returned by [`split_record`].
///
/// - open ~ open: shared fields unify; each row variable is bound to the
///   other side's extra fields extended with a shared fresh row variable.
/// - open ~ closed: the closed record must provide every field the open
///   side demands; its remaining fields close the row.
fn unify_open_records(
    fields1: &[(String, Type)],
    tail1: &Option<Type>,
    fields2: &[(String, Type)],
    tail2: &Option<Type>,
    span: Span,
) -> NuResult<Substitution> {
    let names1: HashSet<&str> = fields1.iter().map(|(n, _)| n.as_str()).collect();
    let names2: HashSet<&str> = fields2.iter().map(|(n, _)| n.as_str()).collect();

    // Shared fields unify pairwise (sorted for determinism).
    let mut shared: Vec<&str> = names1.intersection(&names2).copied().collect();
    shared.sort_unstable();
    let lookup = |fs: &[(String, Type)], name: &str| {
        fs.iter()
            .find(|(n, _)| n == name)
            .map(|(_, t)| t.clone())
            .expect("shared field must exist")
    };
    let mut subst: Substitution = vec![];
    for name in shared {
        let t1f = apply_subst(&lookup(fields1, name), &subst);
        let t2f = apply_subst(&lookup(fields2, name), &subst);
        let s = mgu(&t1f, &t2f, span)?;
        subst = compose_subst(&s, &subst);
    }

    // Fields present on only one side must be absorbed by the other side's
    // row tail.
    let extras = |fs: &[(String, Type)], other: &HashSet<&str>| -> Vec<(String, Type)> {
        fs.iter()
            .filter(|(n, _)| !other.contains(n.as_str()))
            .map(|(n, t)| (n.clone(), apply_subst(t, &subst)))
            .collect()
    };
    let extras1 = extras(fields1, &names2);
    let extras2 = extras(fields2, &names1);

    match (tail1, tail2) {
        (Some(Type::Var(r1)), Some(Type::Var(r2))) => {
            if r1 == r2 {
                // Same row variable on both sides: only unifiable when the
                // field sets already agree (otherwise the row would be
                // ill-formed, the row analogue of an occurs failure).
                if extras1.is_empty() && extras2.is_empty() {
                    return Ok(subst);
                }
                return Err(NuError::TypeError {
                    msg: "Incompatible record types: both sides require additional fields \
                          that cannot be reconciled"
                        .to_string(),
                    span,
                    expected_type: Some(format!(
                        "record with fields {{{}}}",
                        fields1
                            .iter()
                            .map(|(name, _)| name.as_str())
                            .collect::<Vec<_>>()
                            .join(", ")
                    )),
                    found_type: Some(format!(
                        "record with fields {{{}}}",
                        fields2
                            .iter()
                            .map(|(name, _)| name.as_str())
                            .collect::<Vec<_>>()
                            .join(", ")
                    )),
                    similar_names: None,
                });
            }
            let fresh_row = TypeVar::fresh();
            let s = mgu(
                &Type::Var(*r1),
                &Type::record_open(extras2, fresh_row),
                span,
            )?;
            subst = compose_subst(&s, &subst);
            let s = mgu(
                &Type::Var(*r2),
                &Type::record_open(extras1, fresh_row),
                span,
            )?;
            subst = compose_subst(&s, &subst);
            Ok(subst)
        }
        (Some(Type::Var(r)), None) => {
            if let Some((missing, _)) = extras1.first() {
                let available: Vec<String> = fields2.iter().map(|(n, _)| n.clone()).collect();
                return Err(NuError::field_not_found(
                    missing.clone(),
                    span,
                    Some(available),
                ));
            }
            let s = mgu(&Type::Var(*r), &Type::record(extras2), span)?;
            Ok(compose_subst(&s, &subst))
        }
        (None, Some(Type::Var(r))) => {
            if let Some((missing, _)) = extras2.first() {
                let available: Vec<String> = fields1.iter().map(|(n, _)| n.clone()).collect();
                return Err(NuError::field_not_found(
                    missing.clone(),
                    span,
                    Some(available),
                ));
            }
            let s = mgu(&Type::Var(*r), &Type::record(extras1), span)?;
            Ok(compose_subst(&s, &subst))
        }
        // Row tails are always fresh type variables by construction; a
        // residual non-variable tail cannot absorb fields.
        _ => Err(NuError::TypeError {
            msg: "Incompatible record types: the rows cannot be unified".to_string(),
            span,
            expected_type: Some(format!(
                "record with fields {{{}}}",
                fields1
                    .iter()
                    .map(|(name, _)| name.as_str())
                    .collect::<Vec<_>>()
                    .join(", ")
            )),
            found_type: Some(format!(
                "record with fields {{{}}}",
                fields2
                    .iter()
                    .map(|(name, _)| name.as_str())
                    .collect::<Vec<_>>()
                    .join(", ")
            )),
            similar_names: None,
        }),
    }
}

/// Unify a list of type variable / type pairs (common sub-structures).
fn unify_many_app(types1: &[Type], types2: &[Type], span: Span) -> NuResult<Substitution> {
    if types1.len() != types2.len() {
        return Err(NuError::type_mismatch(
            format!("type list of length {}", types1.len()),
            format!("type list of length {}", types2.len()),
            span,
        ));
    }
    let mut subst = vec![];
    for (t1, t2) in types1.iter().zip(types2.iter()) {
        let s = mgu(&apply_subst(t1, &subst), &apply_subst(t2, &subst), span)?;
        subst = compose_subst(&s, &subst);
    }
    Ok(subst)
}

/// Unify two lists of types pairwise.
fn unify_many(types1: &[Type], types2: &[Type], span: Span) -> NuResult<Substitution> {
    if types1.len() != types2.len() {
        return Err(NuError::type_mismatch(
            format!("list of {} types", types1.len()),
            format!("list of {} types", types2.len()),
            span,
        ));
    }
    let mut subst = vec![];
    for (t1, t2) in types1.iter().zip(types2.iter()) {
        let s = mgu(&apply_subst(t1, &subst), &apply_subst(t2, &subst), span)?;
        subst = compose_subst(&s, &subst);
    }
    Ok(subst)
}

/// Create a substitution for a single type variable, with occurs check.
/// Collect every `App(Var(target), args)` from within variant payloads.
fn find_recursive_apps(vs: &[(String, Option<Type>)], target: TypeVar, out: &mut Vec<Vec<Type>>) {
    for (_, payload) in vs {
        if let Some(p) = payload {
            find_recursive_apps_in_type(p, target, out);
        }
    }
}
fn find_recursive_apps_in_type(ty: &Type, target: TypeVar, out: &mut Vec<Vec<Type>>) {
    match ty {
        Type::App { constructor, args } => {
            if let Type::Var(v) = constructor.as_ref() {
                if *v == target {
                    out.push(args.clone());
                }
            }
            find_recursive_apps_in_type(constructor, target, out);
            for a in args {
                find_recursive_apps_in_type(a, target, out);
            }
        }
        Type::Tuple(ts) => {
            for t in ts {
                find_recursive_apps_in_type(t, target, out);
            }
        }
        Type::Variant(vs) => {
            for (_, p) in vs {
                if let Some(p) = p {
                    find_recursive_apps_in_type(p, target, out);
                }
            }
        }
        Type::Function { param, ret, .. } => {
            find_recursive_apps_in_type(param, target, out);
            find_recursive_apps_in_type(ret, target, out);
        }
        _ => {}
    }
}

fn var_subst(v: TypeVar, t: &Type, span: Span) -> NuResult<Substitution> {
    match t {
        Type::Skolem(_) => Ok(vec![(v, t.clone())]),
        Type::Var(v2) if *v2 == v => Ok(vec![]), // t = t
        t => {
            if occurs_in(v, t) {
                return Err(NuError::TypeError {
                    msg: format!(
                        "Infinite type: this expression's type references itself. \
                         This often happens with self-referential definitions \
                         (e.g., a record that contains itself, or `let f = f`). \
                         (Type variable {} occurs in {})",
                        v, t
                    ),
                    span,
                    expected_type: None,
                    found_type: None,
                    similar_names: None,
                });
            }
            Ok(vec![(v, t.clone())])
        }
    }
}

/// Check if a type variable occurs within a type (occurs check).
fn occurs_in(v: TypeVar, t: &Type) -> bool {
    match t {
        Type::Var(v2) => *v2 == v,
        Type::Primitive(_) => false,
        Type::Skolem(_) => false,
        Type::Tuple(ts) => ts.iter().any(|t| occurs_in(v, t)),
        Type::Record(fs) => fs.iter().any(|(_, t)| occurs_in(v, t)),
        // Variant constructors provide a well-founded guard for recursive
        // types (e.g. `type Tree[T] = Node(T, Tree[T]) | Leaf`).
        Type::Variant(_) => false,
        Type::Array(t) => occurs_in(v, t),
        Type::Function { param, ret, .. } => occurs_in(v, param) || occurs_in(v, ret),
        Type::Actor { state, behavior } => occurs_in(v, state) || occurs_in(v, behavior),
        Type::App { constructor, args } => {
            occurs_in(v, constructor) || args.iter().any(|a| occurs_in(v, a))
        }
        Type::Reference { inner, .. } => occurs_in(v, inner),
        Type::Scheme { vars, body } => !vars.contains(&v) && occurs_in(v, body),
        Type::Nominal { underlying, .. } => occurs_in(v, underlying),
    }
}

// ---------------------------------------------------------------------------
// Instantiation
// ---------------------------------------------------------------------------

/// Instantiate a scheme by replacing all bound type variables with fresh ones.
fn instantiate(ty: &Type) -> Type {
    match ty {
        Type::Scheme { vars, body } => {
            let subst: Substitution = vars
                .iter()
                .map(|v| (*v, Type::Var(TypeVar::fresh())))
                .collect();
            apply_subst(body, &subst)
        }
        _ => ty.clone(),
    }
}

// ---------------------------------------------------------------------------
// TypeChecker
// ---------------------------------------------------------------------------

/// Metadata for a registered typeclass.
#[derive(Debug, Clone)]
#[allow(dead_code)]
pub struct ClassInfo {
    pub type_params: Vec<String>,
    pub super_classes: Vec<String>,
    pub methods: Vec<ClassMethod>,
}

/// Hindley-Milner type checker implementing Algorithm W.
pub struct TypeChecker {
    /// Registered typeclass declarations.
    pub class_table: FxHashMap<String, ClassInfo>,
    /// Registered instance declarations: (class_name, type_name) → methods.
    pub instance_table: FxHashMap<(String, String), Vec<ImplMethod>>,
    /// Inferred types for top-level declarations, populated by `check_module`.
    /// Keyed by declaration name; for functions this is the function type,
    /// for let bindings this is the inferred value type.
    pub inferred_decl_types: FxHashMap<String, Type>,
    /// Contextual `given` bindings: name → (type annotation, value expression).
    pub given_bindings: FxHashMap<String, (Option<Type>, Expr)>,
    /// Functions with `using` params: fn_name → using param names.
    pub fn_using_params: FxHashMap<String, Vec<String>>,
    /// Self-referencing type variables from recursive ADT types.
    /// Must not be generalized — they must stay identical across
    /// constructor instantiations for structural unification.
    pub rigid_vars: FxHashSet<TypeVar>,
    /// When true, `check_module` continues past per-declaration type errors,
    /// collecting them in `collected_errors` instead of aborting at the first.
    pub collect_errors: bool,
    /// Errors collected when `collect_errors` is set (empty otherwise).
    pub collected_errors: Vec<crate::types::NuError>,
}

/// Pre-computed class and instance tables extracted from an AST module.
/// Shared between the typechecker and HIR lowering so both can resolve
/// typeclass method calls.
#[derive(Debug, Clone, Default)]
pub struct ClassTables {
    pub class_table: FxHashMap<String, ClassInfo>,
    pub instance_table: FxHashMap<(String, String), Vec<ImplMethod>>,
}

/// Build class and instance tables by scanning a module's declarations.
/// This is the shared extraction logic — `TypeChecker::register_class_decls`
/// delegates to it, and `hir_lower::lower_module` calls it independently.
pub fn build_class_tables(module: &AstModule) -> ClassTables {
    let mut tables = ClassTables::default();
    for decl in flatten_decls(&module.decls) {
        match decl {
            Decl::Class {
                name,
                type_params,
                super_classes,
                methods,
                ..
            } => {
                tables.class_table.insert(
                    name.clone(),
                    ClassInfo {
                        type_params: type_params.clone(),
                        super_classes: super_classes.clone(),
                        methods: methods.clone(),
                    },
                );
            }
            Decl::Impl {
                class_name,
                for_type,
                methods,
                ..
            } => {
                let type_key = format!("{}", for_type);
                tables
                    .instance_table
                    .insert((class_name.clone(), type_key), methods.clone());
            }
            _ => {}
        }
    }
    tables
}

/// Recursively splice any `Decl::Module { decls, .. }` in place with its own
/// contents, in source order, leaving every other declaration untouched.
///
/// This mirrors the stable compiler's `collect_functions`/`compile_decl`,
/// which recurse into nested modules and register their contents in the same
/// flat, unqualified namespace as top-level decls — modules are a namespacing
/// construct only, with no enforced visibility boundary.
fn flatten_decls(decls: &[Decl]) -> Vec<&Decl> {
    let mut out = Vec::with_capacity(decls.len());
    for decl in decls {
        match decl {
            Decl::Module { decls: inner, .. } => out.extend(flatten_decls(inner)),
            _ => out.push(decl),
        }
    }
    out
}

impl TypeChecker {
    /// Create a new type checker with an empty context.
    pub fn new() -> Self {
        TypeChecker {
            class_table: FxHashMap::default(),
            instance_table: FxHashMap::default(),
            inferred_decl_types: FxHashMap::default(),
            given_bindings: FxHashMap::default(),
            fn_using_params: FxHashMap::default(),
            rigid_vars: FxHashSet::default(),
            collect_errors: false,
            collected_errors: Vec::new(),
        }
    }

    /// Type-check an entire module, returning the type of the last declaration.
    ///
    pub fn register_class_decls(&mut self, module: &AstModule) {
        let tables = build_class_tables(module);
        self.class_table = tables.class_table;
        self.instance_table = tables.instance_table;
    }

    /// Type-check an entire module, returning the type of the last declaration.
    pub fn check_module(&mut self, module: &AstModule) -> NuResult<Type> {
        self.register_class_decls(module);
        let mut ctx = TypeContext::new();
        let mut last_type = Type::unit();
        for decl in flatten_decls(&module.decls) {
            let (s, ty) = match self.infer_decl(&ctx, decl) {
                Ok(ok) => ok,
                Err(e) => {
                    if self.collect_errors {
                        self.collected_errors.push(e);
                        continue;
                    }
                    return Err(e);
                }
            };
            ctx = apply_subst_to_ctx(&ctx, &s);
            let final_ty = apply_subst(&ty, &s);
            match decl {
                Decl::Function {
                    name, using_params, ..
                } => {
                    if !using_params.is_empty() {
                        self.fn_using_params.insert(
                            name.clone(),
                            using_params.iter().map(|p| p.name.clone()).collect(),
                        );
                    }
                    self.inferred_decl_types
                        .insert(name.clone(), final_ty.clone());
                    let gen_ty = self.do_generalize(&ctx, &final_ty);
                    ctx.bind(name.clone(), gen_ty, Capability::Ref, false);
                }
                Decl::Actor { name, .. } => {
                    ctx.bind(name.clone(), final_ty.clone(), Capability::Ref, false);
                }
                Decl::StateMachine { name, .. } => {
                    ctx.bind(name.clone(), final_ty.clone(), Capability::Ref, false);
                }
                Decl::Extern { funcs, .. } => {
                    for func in funcs {
                        let param_types: Vec<Type> =
                            func.params.iter().map(|(_, t)| t.clone()).collect();
                        let param_ty = if param_types.len() == 1 {
                            param_types[0].clone()
                        } else {
                            Type::Tuple(param_types)
                        };
                        let func_ty = Type::Function {
                            param: Box::new(param_ty),
                            ret: Box::new(func.ret.clone()),
                            effect: EffectRow::singleton(Effect::FFI),
                            cap: Capability::Ref,
                        };
                        ctx.bind(func.name.clone(), func_ty, Capability::Ref, false);
                    }
                }
                Decl::Workflow { name, .. } => {
                    ctx.bind(name.clone(), final_ty.clone(), Capability::Ref, false);
                }
                Decl::Agent { name, .. } => {
                    ctx.bind(name.clone(), final_ty.clone(), Capability::Ref, false);
                }
                Decl::VariantType { variants, .. } => {
                    // Bind each constructor (SPEC2 §3.4.1): a constructor with
                    // a payload is a function from the payload type to the
                    // variant type; a nullary constructor is a plain value of
                    // the variant type. Declared type parameters (e.g. `T` in
                    // `Option[T]`) stay free in the payload types parsed from
                    // the declaration and are generalized per constructor, so
                    // each use instantiates them fresh.
                    let variant_ty = Type::Variant(variants.clone());
                    Self::collect_recursive_vars(&variant_ty, &mut self.rigid_vars);
                    for (ctor_name, payload) in variants {
                        let ctor_ty = match payload {
                            Some(payload_ty) => Type::Function {
                                param: Box::new(payload_ty.clone()),
                                ret: Box::new(variant_ty.clone()),
                                effect: EffectRow::empty(),
                                cap: Capability::Ref,
                            },
                            None => variant_ty.clone(),
                        };
                        let gen_ty = self.do_generalize(&ctx, &ctor_ty);
                        ctx.bind(ctor_name.clone(), gen_ty, Capability::Ref, false);
                    }
                }

                Decl::Class { name, .. } => {
                    // Bind class name as a type-level marker; runtime value
                    // is unit — classes constrain type variables at compile
                    // time and have no runtime representation.
                    ctx.bind(name.clone(), Type::unit(), Capability::Ref, false);
                }
                Decl::Impl {
                    class_name,
                    for_type,
                    ..
                } => {
                    // Bind the instance dictionary under a synthetic name
                    // so instance-lookup can resolve it at call sites.
                    let dict_name = format!("_impl_{}_{}", class_name, for_type);
                    ctx.bind(dict_name, final_ty.clone(), Capability::Ref, false);
                }
                Decl::LetBinding { name, .. } | Decl::Signal { name, .. } => {
                    self.inferred_decl_types
                        .insert(name.clone(), final_ty.clone());
                    let gen_ty = self.do_generalize(&ctx, &final_ty);
                    ctx.bind(name.clone(), gen_ty, Capability::Ref, false);
                }

                _ => {}
            }
            last_type = final_ty;
        }
        // Verify behavior contracts for actors with `implements` clauses.
        if let Err(e) = self.verify_behavior_contracts(module) {
            if self.collect_errors {
                self.collected_errors.push(e);
            } else {
                return Err(e);
            }
        }
        Ok(last_type)
    }

    /// Verify that every actor declaring `implements <contract>` actually
    /// provides all required handler behaviors with compatible signatures.
    fn verify_behavior_contracts(&self, module: &AstModule) -> NuResult<()> {
        for decl in &module.decls {
            self.verify_decl_contracts(decl)?;
        }
        Ok(())
    }

    fn verify_decl_contracts(&self, decl: &Decl) -> NuResult<()> {
        match decl {
            Decl::Actor {
                name,
                behaviors,
                implements,
                span,
                ..
            } => {
                if let Some(contract_name) = implements {
                    let contract = crate::stdlib::lookup_contract(contract_name);
                    match contract {
                        Some(c) => {
                            for &(handler_name, param_count) in c.required_handlers {
                                let found = behaviors.iter().any(|b| {
                                    b.name == handler_name && b.params.len() == param_count
                                });
                                if !found {
                                    let msg = format!(
                                        "actor '{}' declares it implements '{}' but is missing required handler '{}' (expects {} parameter(s))",
                                        name, contract_name, handler_name, param_count
                                    );
                                    return Err(NuError::TypeError {
                                        msg,
                                        span: *span,
                                        expected_type: None,
                                        found_type: None,
                                        similar_names: None,
                                    });
                                }
                            }
                        }
                        None => {
                            let msg = format!(
                                "unknown behavior contract '{}' in actor '{}'",
                                contract_name, name
                            );
                            return Err(NuError::TypeError {
                                msg,
                                span: *span,
                                expected_type: None,
                                found_type: None,
                                similar_names: None,
                            });
                        }
                    }
                }
            }
            Decl::Module { decls, .. } => {
                for d in decls {
                    self.verify_decl_contracts(d)?;
                }
            }
            _ => {}
        }
        Ok(())
    }

    /// Infer the type of a declaration.
    pub(crate) fn infer_decl(
        &mut self,
        ctx: &TypeContext,
        decl: &Decl,
    ) -> NuResult<(Substitution, Type)> {
        match decl {
            Decl::CrdtDecl {
                name: _name,
                span: _span,
                ..
            } => Ok((Vec::new(), Type::unit())),
            Decl::Function {
                name,
                type_param_constraints,
                params,
                using_params,
                ret_type,
                error_type,
                effect,
                requires,
                ensures,
                body,
                span,
                ..
            } => {
                // When `! ErrorType` syntax is used, wrap the declared return
                // type with `Result[ret_type, error_type]`.
                let ret_type: Option<Type> = match (ret_type, error_type) {
                    (Some(ok_ty), Some(err_ty)) => Some(Type::Variant(vec![
                        ("Ok".to_string(), Some(ok_ty.clone())),
                        ("Error".to_string(), Some(err_ty.clone())),
                    ])),
                    _ => ret_type.clone(),
                };

                // Skolemize type parameters: replace each type parameter's
                // variable with a rigid Skolem constant so the function body
                // cannot pin a generic type to a concrete type (e.g.
                // `fn fresh[T]() -> T { 0 - 1 }` must be rejected at the
                // definition, not at the call site).
                let mut skolem_map: FxHashMap<TypeVar, Type> = FxHashMap::default();
                // Collect type parameter variables from constrained params.
                for (_, tv, _) in type_param_constraints {
                    skolem_map.entry(*tv).or_insert_with(|| Type::Skolem(tv.0));
                }
                // Also collect from return type and param annotations for
                // unconstrained type params (e.g., plain `[T]`).
                let mut collect_vars = |ty: &Type| {
                    collect_type_vars(ty, &mut skolem_map);
                };
                if let Some(rt) = &ret_type {
                    collect_vars(rt);
                }
                for p in params {
                    if let Some(t) = &p.ty {
                        collect_vars(t);
                    }
                }

                let subst_skolem = |ty: &Type| -> Type { subst_type_vars_with(ty, &skolem_map) };

                /// Collect all type variables from a function signature type into the
                /// skolem map, creating fresh Skolem entries for each unique TypeVar.
                /// Only collects "standalone" type vars (type parameter usages), not
                /// recursive reference variables (TypeVars used as App constructors)
                /// or type vars buried inside type definitions (variant payloads, etc.).
                fn collect_type_vars(ty: &Type, map: &mut FxHashMap<TypeVar, Type>) {
                    match ty {
                        Type::Var(v) => {
                            map.entry(*v).or_insert_with(|| Type::Skolem(v.0));
                        }
                        Type::Tuple(ts) => {
                            for t in ts {
                                collect_type_vars(t, map);
                            }
                        }
                        Type::Array(t) => collect_type_vars(t, map),
                        Type::Nominal { .. } => {} // opaque — skip underlying to avoid cycles
                        Type::App {
                            constructor: _,
                            args,
                        } => {
                            // Only collect from args — the constructor is a type name,
                            // not a type parameter usage.
                            for a in args {
                                collect_type_vars(a, map);
                            }
                        }
                        Type::Reference { inner, .. } => collect_type_vars(inner, map),
                        Type::Scheme { body, .. } => collect_type_vars(body, map),
                        // Don't recurse into type definitions (Variant, Record, Actor, Nominal)
                        // — those contain type-structure variables, not type-parameter usages.
                        _ => {}
                    }
                }
                // Skolemize the declared return type (if any).
                let ret_type: Option<Type> = ret_type.map(|rt| subst_skolem(&rt));
                let mut param_types = vec![];
                for p in params {
                    let param_ty = &p.ty;
                    let pty = match param_ty {
                        Some(t) => subst_skolem(t),
                        None => Type::Var(TypeVar::fresh()),
                    };
                    param_types.push(pty);
                }
                for p in using_params {
                    let using_ty = &p.ty;
                    let uty = match using_ty {
                        Some(t) => t.clone(),
                        None => Type::Var(TypeVar::fresh()),
                    };
                    param_types.push(uty);
                }

                // Preliminary parameter type for the function binding
                let param_ty = if param_types.len() == 1 {
                    param_types[0].clone()
                } else {
                    Type::Tuple(param_types.clone())
                };

                // Fresh return type variable so the function can refer to itself
                // recursively before its body is inferred.
                let ret_var = Type::Var(TypeVar::fresh());
                let declared_effect = effect.clone().unwrap_or_else(EffectRow::empty);
                let recursive_func_ty = Type::Function {
                    param: Box::new(param_ty.clone()),
                    ret: Box::new(ret_var.clone()),
                    effect: declared_effect.clone(),
                    cap: Capability::Ref,
                };

                let mut new_ctx = ctx.clone();
                // Bind the function name so recursive calls resolve.
                new_ctx.bind(name.clone(), recursive_func_ty, Capability::Ref, false);
                // Bind parameters
                for (p, pty) in params.iter().zip(param_types.iter()) {
                    let pcap = p.cap.unwrap_or(Capability::Ref);
                    new_ctx.bind(p.name.clone(), pty.clone(), pcap, false);
                }
                for up in using_params {
                    let uty = match &up.ty {
                        Some(t) => t.clone(),
                        None => Type::Var(TypeVar::fresh()),
                    };
                    let upcap = up.cap.unwrap_or(Capability::Ref);
                    new_ctx.bind(up.name.clone(), uty, upcap, false);
                }
                // Inject typeclass constraints from type parameter annotations
                // so method calls on constrained type vars resolve (B.3).
                for (_, tv, class_names) in type_param_constraints {
                    for cn in class_names {
                        new_ctx.add_constraint(*tv, cn);
                        // Also map the constraint by skolem ID so lookups
                        // on the skolemized receiver resolve correctly.
                        if let Some(Type::Skolem(sk_id)) = skolem_map.get(tv) {
                            new_ctx.add_constraint(TypeVar(*sk_id), cn);
                        }
                    }
                }
                let (s1, body_ty) = self.infer_expr(&new_ctx, body)?;

                // Contract predicates must be Bool-typed. Postconditions see
                // `result` bound to the inferred return type.
                for req in requires {
                    let (_s, req_ty) = self.infer_expr(&new_ctx, req)?;
                    let _ = mgu(&req_ty, &Type::bool(), *span)?;
                }
                let mut ensures_ctx = new_ctx.clone();
                ensures_ctx.bind(
                    "result".to_string(),
                    body_ty.clone(),
                    Capability::Ref,
                    false,
                );
                for ens in ensures {
                    let (_s, ens_ty) = self.infer_expr(&ensures_ctx, ens)?;
                    let _ = mgu(&ens_ty, &Type::bool(), *span)?;
                }

                // Unify the preliminary return variable with the inferred body type
                let s_rec = mgu(&apply_subst(&ret_var, &s1), &body_ty, *span)?;
                let s1 = compose_subst(&s_rec, &s1);

                // Unify with declared return type if present
                let s2 = match &ret_type {
                    Some(rt) => {
                        let body_subst = apply_subst(&body_ty, &s1);
                        let rt_subst = apply_subst(rt, &s1);
                        mgu(&body_subst, &rt_subst, *span)?
                    }
                    None => vec![],
                };
                let s_combined = compose_subst(&s2, &s1);

                // Build final function type
                let param_ty = apply_subst(&param_ty, &s_combined);
                let ret_ty = apply_subst(&body_ty, &s_combined);
                let func_ty = Type::Function {
                    param: Box::new(param_ty),
                    ret: Box::new(ret_ty),
                    effect: declared_effect.clone(),
                    cap: Capability::Ref,
                };

                // The function is generalized when bound into the module
                // context (see `check_module`); the raw type is returned so
                // the caller can substitute it into the module's type.
                Ok((s_combined, func_ty))
            }
            Decl::TypeAlias { .. } => Ok((vec![], Type::unit())),
            Decl::RecordType { .. } => Ok((vec![], Type::unit())),
            Decl::VariantType { .. } => Ok((vec![], Type::unit())),
            Decl::EffectDecl { .. } => Ok((vec![], Type::unit())),
            Decl::Actor {
                name,
                behaviors,
                events,
                migrations,
                span,
                ..
            } => self.infer_actor_decl(ctx, name, behaviors, events, migrations, *span),
            Decl::StateMachine {
                name,
                states,
                events,
                entry_hooks,
                exit_hooks,
                span,
            } => {
                // Type-check the desugared form exactly like an actor (the
                // desugar targets the ordinary actor machinery).
                let actor = crate::ast::desugar_state_machine(
                    name,
                    states,
                    events,
                    entry_hooks,
                    exit_hooks,
                    *span,
                );
                self.infer_decl(ctx, &actor)
            }
            Decl::Agent { .. } => {
                // An agent declaration is an opaque module-level binding with a
                // synthetic actor type, just like actors and workflows.
                let agent_ty = Type::Actor {
                    state: Box::new(Type::Var(TypeVar::fresh())),
                    behavior: Box::new(Type::Var(TypeVar::fresh())),
                };
                Ok((vec![], agent_ty))
            }
            Decl::Extern { funcs, span, .. } => {
                for func in funcs {
                    for (_name, ty) in &func.params {
                        self.validate_ffi_type(ty, *span)?;
                    }
                    self.validate_ffi_type(&func.ret, *span)?;
                }
                Ok((vec![], Type::unit()))
            }

            Decl::Module { decls, .. } => {
                let mut ctx = ctx.clone();
                let mut last = Type::unit();
                let mut all_subst = vec![];
                for d in decls {
                    let (s, ty) = self.infer_decl(&ctx, d)?;
                    ctx = apply_subst_to_ctx(&ctx, &s);
                    all_subst = compose_subst(&s, &all_subst);
                    last = ty;
                }
                Ok((all_subst, last))
            }
            Decl::NamedHandler { .. } => {
                // NamedHandler declarations are resolved at parse time
                // (handlers inlined into handle expressions). They carry
                // no runtime type and produce no code.
                Ok((vec![], Type::unit()))
            }
            Decl::Workflow {
                name: _,
                input,
                items,
                span: _,
                ..
            } => {
                // A workflow declaration is an opaque module-level binding with a
                // synthetic actor type. Each step body is type-checked in a context
                // extended with the workflow input, if one is declared.
                let mut workflow_ctx = ctx.clone();
                if let Some((input_name, input_ty)) = input {
                    workflow_ctx.bind(input_name.clone(), input_ty.clone(), Capability::Ref, false);
                }
                for item in items {
                    match item {
                        crate::ast::WorkflowItem::Step(step) => {
                            let (_s, _body_ty) = self.infer_expr(&workflow_ctx, &step.body)?;
                            if let Some(comp_expr) = &step.compensate {
                                let (_s, _comp_ty) = self.infer_expr(&workflow_ctx, comp_expr)?;
                            }
                        }
                        crate::ast::WorkflowItem::Parallel(branches) => {
                            for step in branches {
                                let (_s, _body_ty) = self.infer_expr(&workflow_ctx, &step.body)?;
                                if let Some(comp_expr) = &step.compensate {
                                    let (_s, _comp_ty) =
                                        self.infer_expr(&workflow_ctx, comp_expr)?;
                                }
                            }
                        }
                    }
                }
                let workflow_ty = Type::Actor {
                    state: Box::new(Type::Var(TypeVar::fresh())),
                    behavior: Box::new(Type::Var(TypeVar::fresh())),
                };
                Ok((vec![], workflow_ty))
            }

            Decl::LetBinding {
                name: _,
                type_ann,
                value,
                span,
                ..
            } => {
                let (s1, val_ty) = self.infer_expr(ctx, value)?;
                let s1 = if let Some(ann_ty) = type_ann {
                    let s_ann = mgu(&apply_subst(&val_ty, &s1), ann_ty, *span)?;
                    compose_subst(&s_ann, &s1)
                } else {
                    s1
                };
                let final_ty = apply_subst(&val_ty, &s1);
                Ok((s1, final_ty))
            }
            Decl::Signal { init, ty, span, .. } => {
                let (s1, val_ty) = self.infer_expr(ctx, init)?;
                let s_ann = mgu(&apply_subst(&val_ty, &s1), ty, *span)?;
                let s1 = compose_subst(&s_ann, &s1);
                let final_ty = apply_subst(&val_ty, &s1);
                Ok((s1, final_ty))
            }
            Decl::Import { .. } => Ok((vec![], Type::unit())),
            Decl::Database { .. } => Ok((vec![], Type::unit())),
            Decl::Given {
                name, ty, value, ..
            } => {
                let (s_val, val_ty) = self.infer_expr(ctx, value)?;
                self.given_bindings
                    .insert(name.clone(), (ty.clone(), value.clone()));
                Ok((s_val, val_ty))
            }
            Decl::Class { .. } => Ok((vec![], Type::unit())),
            Decl::Impl {
                class_name: _,
                for_type,
                methods,
                ..
            } => {
                // Build a dictionary record type from method signatures.
                // Each method becomes a field whose type is
                //   fn(for_type, declared_params...) -> declared_return_type.
                // Method body type-checking is deferred; the dictionary
                // record is bound into scope at its synthetic name so
                // instance-lookup (B.4) can resolve it.
                let mut field_types: Vec<(String, Type)> = Vec::new();
                for method in methods {
                    let mut param_types = vec![for_type.clone()]; // self
                                                                  // Skip the first param (self) — it is already
                                                                  // covered by for_type above.
                    for (_, pty) in &method.params[1..] {
                        param_types.push(pty.clone());
                    }
                    let param_ty = if param_types.len() == 1 {
                        param_types[0].clone()
                    } else {
                        Type::Tuple(param_types)
                    };
                    let func_ty = Type::Function {
                        param: Box::new(param_ty),
                        ret: Box::new(method.return_type.clone()),
                        effect: EffectRow::empty(),
                        cap: Capability::Ref,
                    };
                    field_types.push((method.name.clone(), func_ty));
                }
                Ok((vec![], Type::Record(field_types)))
            }
        }
    }

    /// Infer the type of an expression (Algorithm W).
    /// Returns (substitution, inferred_type).
    pub fn infer_expr(&mut self, ctx: &TypeContext, expr: &Expr) -> NuResult<(Substitution, Type)> {
        match expr {
            Expr::FString(parts, _) => {
                let mut subst = Substitution::new();
                for part in parts {
                    let (s, _) = self.infer_expr(ctx, part)?;
                    subst = compose_subst(&subst, &s);
                }
                return Ok((subst, Type::string()));
            }
            // Literals: exact primitive type
            Expr::Literal(lit, span) => self.infer_literal(lit, *span),

            // Variables: look up in context, instantiate scheme
            Expr::Var(name, span) => self.infer_var(ctx, name, *span),

            // Lambda: introduce fresh type vars for params, infer body
            Expr::Lambda {
                params,
                body,
                effect,
                span,
                ..
            } => self.infer_lambda(ctx, params, body, effect.as_ref(), *span),

            // Application: infer function, infer arg, unify, return result
            Expr::App { func, args, span } => self.infer_app(ctx, func, args, *span),

            // Let binding: infer value, generalize, extend context, infer body
            Expr::Let {
                name,
                ty,
                value,
                body,
                mutable,
                span,
                let_in: _,
            } => self.infer_let(ctx, name, ty.as_ref(), value, body, *mutable, *span),

            // Let-rec: recursive binding
            Expr::LetRec {
                name,
                params,
                value,
                body,
                span,
            } => self.infer_letrec(ctx, name, params, value, body, *span),

            // If: condition must be Bool, branches must match
            Expr::If {
                cond,
                then_branch,
                else_branch,
                span,
            } => self.infer_if(ctx, cond, then_branch, else_branch.as_ref(), *span),
            // Binary operators: type-specific rules
            Expr::Binary {
                op,
                left,
                right,
                span,
            } => self.infer_binary(ctx, *op, left, right, *span),

            // Unary operators
            Expr::Unary { op, expr, span } => self.infer_unary(ctx, *op, expr, *span),

            // Tuple: infer each element
            Expr::Tuple(exprs, span) => self.infer_tuple(ctx, exprs, *span),

            // Record literal: infer each field
            Expr::Record(fields, span) => self.infer_record(ctx, fields, *span),

            // Record update: { base .. field = val, ... }
            Expr::RecordUpdate { base, fields, span } => {
                self.infer_record_update(ctx, base, fields, *span)
            }

            // Field access: look up field in record type
            Expr::FieldAccess { expr, field, span } => {
                self.infer_field_access(ctx, expr, field, *span)
            }

            // Array literal
            Expr::Array(elems, span) => self.infer_array(ctx, elems, *span),

            // Array index
            Expr::Index { arr, idx, span } => self.infer_index(ctx, arr, idx, *span),

            // Pattern matching
            Expr::Match {
                scrutinee,
                arms,
                span,
            } => self.infer_match(ctx, scrutinee, arms, *span),

            // Block: sequence of expressions
            Expr::Block { exprs, span } => self.infer_block(ctx, exprs, *span),

            // Par: independence annotation, sequential block semantics.
            Expr::Par { exprs, span } => self.infer_block(ctx, exprs, *span),

            // Spawn actor
            Expr::Spawn {
                actor_type,
                target_node,
                span,
                ..
            } => self.infer_spawn(ctx, actor_type, target_node.as_deref(), *span),

            // Send message
            Expr::Send {
                actor,
                behavior,
                args,
                span,
                ..
            } => self.infer_send(ctx, actor, behavior, args, *span),

            // Ask request
            Expr::Ask {
                actor,
                behavior: _,
                args: _,
                span,
                ..
            } => self.infer_ask(ctx, actor, *span),

            // Receive
            Expr::Receive { after, span, .. } => {
                // The optional `after ms => body` clause is checked: the
                // timeout must be an Int (milliseconds); the timeout body is
                // inferred like any other expression.
                let mut subst = vec![];
                if let Some((ms, timeout_body)) = after {
                    let (s1, ms_ty) = self.infer_expr(ctx, ms)?;
                    let s2 = mgu(&ms_ty, &Type::int(), *span)?;
                    subst = compose_subst(&s2, &s1);
                    let ctx_sub = apply_subst_to_ctx(ctx, &subst);
                    let (s3, _) = self.infer_expr(&ctx_sub, timeout_body)?;
                    subst = compose_subst(&s3, &subst);
                }
                let ret_ty = Type::Var(TypeVar::fresh());
                Ok((subst, ret_ty))
            }

            // Self reference
            Expr::SelfRef(_) => Ok((vec![], Type::Var(TypeVar::fresh()))),

            // Virtual actor reference: Grain("Type", key).
            // The key expression is checked; the whole expression has actor type.
            Expr::GrainRef { key, .. } => {
                let (subst, _key_ty) = self.infer_expr(ctx, key)?;
                let actor_ty = Type::Actor {
                    state: Box::new(Type::Var(TypeVar::fresh())),
                    behavior: Box::new(Type::Var(TypeVar::fresh())),
                };
                Ok((subst, actor_ty))
            }

            // Perform effect
            Expr::Perform {
                effect,
                op: _,
                args,
                span,
            } => self.infer_perform(ctx, effect, args, *span),
            // Emit event — check against entity's declared events if in an entity context
            Expr::Emit { event, args, span } => {
                // Validate against entity event declarations if available
                if let Some(ref entity_events) = ctx.entity_events {
                    let declared = entity_events.iter().find(|(name, _)| name == event);
                    match declared {
                        None => {
                            let available: Vec<_> =
                                entity_events.iter().map(|(n, _)| n.as_str()).collect();
                            let available_text = if available.is_empty() {
                                "(none)".to_string()
                            } else {
                                available.join(", ")
                            };
                            return Err(NuError::TypeError {
                                msg: format!(
                                    "Unknown event '{}'. Available events: {}",
                                    event, available_text
                                ),
                                span: *span,
                                expected_type: Some("declared event name".to_string()),
                                found_type: Some(event.clone()),
                                similar_names: if available.is_empty() {
                                    None
                                } else {
                                    Some(available.iter().map(|name| (*name).to_string()).collect())
                                },
                            });
                        }
                        Some(params) => {
                            if args.len() != params.1.len() {
                                let plural =
                                    |n: usize| if n == 1 { "argument" } else { "arguments" };
                                let expected_desc =
                                    format!("{} {}", params.1.len(), plural(params.1.len()));
                                let found_desc = format!("{} {}", args.len(), plural(args.len()));
                                return Err(NuError::TypeError {
                                    msg: format!(
                                        "Event '{}' expects {}, got {}",
                                        event, expected_desc, found_desc
                                    ),
                                    span: *span,
                                    expected_type: Some(expected_desc),
                                    found_type: Some(found_desc),
                                    similar_names: None,
                                });
                            }
                        }
                    }
                }
                let mut subst = Vec::new();
                for arg in args {
                    let ctx_sub = apply_subst_to_ctx(ctx, &subst);
                    let (s, _ty) = self.infer_expr(&ctx_sub, arg)?;
                    subst = compose_subst(&s, &subst);
                }
                Ok((subst, Type::unit()))
            }

            // Handle effect
            Expr::Handle {
                body,
                handlers,
                span,
            } => self.infer_handle(ctx, body, handlers, *span),

            // Migrate actor
            Expr::Migrate {
                actor,
                node: _,
                span,
            } => {
                let (s1, actor_ty) = self.infer_expr(ctx, actor)?;
                // Actor must be an actor type
                match &actor_ty {
                    Type::Actor { .. } => Ok((s1, actor_ty)),
                    _ => {
                        let actor_var = TypeVar::fresh();
                        let s2 = mgu(
                            &apply_subst(&actor_ty, &s1),
                            &Type::Actor {
                                state: Box::new(Type::Var(actor_var)),
                                behavior: Box::new(Type::Var(TypeVar::fresh())),
                            },
                            *span,
                        )?;
                        let actor_subst = apply_subst(&actor_ty, &compose_subst(&s2, &s1));
                        Ok((compose_subst(&s2, &s1), actor_subst))
                    }
                }
            }

            // Capability annotation
            Expr::CapAnnotate { expr, cap, span: _ } => {
                let (s, ty) = self.infer_expr(ctx, expr)?;
                // Wrap in reference type with the given capability
                let ref_ty = Type::Reference {
                    cap: *cap,
                    inner: Box::new(apply_subst(&ty, &s)),
                };
                Ok((s, ref_ty))
            }

            // Consume expression: consume x — explicit move, type is type of x
            Expr::Consume { expr, .. } => self.infer_expr(ctx, expr),

            // Recover expression: recover { body } — isolated scope
            Expr::Recover { body, .. } => self.infer_expr(ctx, body),
            // Type annotation
            Expr::TypeAnnotate { expr, ty, span } => {
                let (s1, inferred) = self.infer_expr(ctx, expr)?;
                let s2 = mgu(&apply_subst(&inferred, &s1), ty, *span)?;
                Ok((
                    compose_subst(&s2, &s1),
                    apply_subst(ty, &compose_subst(&s2, &s1)),
                ))
            }

            // Pipe operator: x |> f  ===  f(x), and x |> f(a, b) === f(x, a, b)
            Expr::Pipe { left, right, span } => {
                let (s1, left_ty) = self.infer_expr(ctx, left)?;
                let ctx1 = apply_subst_to_ctx(ctx, &s1);

                // If the right side is already a function application, prepend
                // the piped value as the first argument. This matches the
                // compiler's pipe lowering and supports multi-arg functions.
                if let Expr::App { func, args, .. } = right.as_ref() {
                    let mut new_args = vec![left.as_ref().clone()];
                    new_args.extend(args.iter().cloned());
                    let app = Expr::App {
                        func: func.clone(),
                        args: new_args,
                        span: *span,
                    };
                    let (s2, ty) = self.infer_expr(&ctx1, &app)?;
                    let final_subst = compose_subst(&s2, &s1);
                    return Ok((final_subst.clone(), apply_subst(&ty, &final_subst)));
                }

                let (s2, right_ty) = self.infer_expr(&ctx1, right)?;
                // right should be a function taking left's type
                let result_var = Type::Var(TypeVar::fresh());
                let expected = Type::Function {
                    param: Box::new(apply_subst(&left_ty, &compose_subst(&s2, &s1))),
                    ret: Box::new(result_var.clone()),
                    effect: EffectRow::empty(),
                    cap: Capability::Ref,
                };
                let s3 = mgu(&apply_subst(&right_ty, &s2), &expected, *span)?;
                let final_subst = compose_subst(&s3, &compose_subst(&s2, &s1));
                Ok((final_subst.clone(), apply_subst(&result_var, &final_subst)))
            }

            // For comprehension
            Expr::For {
                var,
                iterable,
                body,
                span,
            } => self.infer_for(ctx, var, iterable, body, *span),
            Expr::While { cond, body, span } => self.infer_while(ctx, cond, body, *span),

            // Return
            Expr::Return(expr, _span) => {
                if let Some(e) = expr {
                    let (s, _) = self.infer_expr(ctx, e)?;
                    Ok((s, Type::Primitive(PrimitiveType::Never)))
                } else {
                    Ok((vec![], Type::Primitive(PrimitiveType::Never)))
                }
            }

            // Break
            Expr::Break(..) => {
                let fresh = Type::Var(TypeVar::fresh());
                Ok((vec![], fresh))
            }

            // Assignment: target must be a reference, OR a mutable local
            Expr::Assign {
                target,
                value,
                span,
            } => {
                // Check for mutable local variable reassignment first:
                // `var x = 0; x = 5` reassigns the mutable binding `x`.
                if let Expr::Var(name, _) = target.as_ref() {
                    if let Some((binding_ty, _cap, is_mutable)) = ctx.lookup(name) {
                        if *is_mutable {
                            let binding_ty = instantiate(binding_ty);
                            let (s1, value_ty) = self.infer_expr(ctx, value)?;
                            // Unify value type with binding type directly (no Ref wrapper)
                            let s2 = mgu(&apply_subst(&value_ty, &s1), &binding_ty, *span)?;
                            let final_subst = compose_subst(&s2, &s1);
                            return Ok((final_subst, Type::unit()));
                        }
                    }
                    // Not a mutable binding — fall through to error below.
                }

                let (s1, target_ty) = self.infer_expr(ctx, target)?;
                let ctx1 = apply_subst_to_ctx(ctx, &s1);
                let (s2, value_ty) = self.infer_expr(&ctx1, value)?;
                // Unify target (should be a reference) with value
                let target_ty_resolved = apply_subst(&target_ty, &compose_subst(&s2, &s1));
                let expected_ref = Type::Reference {
                    cap: Capability::Ref,
                    inner: Box::new(apply_subst(&value_ty, &s2)),
                };
                let s3 = match mgu(&target_ty_resolved, &expected_ref, *span) {
                    Ok(s) => s,
                    Err(_) => {
                        // Produce a clearer error for simple variable assignments
                        if let Expr::Var(name, _) = target.as_ref() {
                            return Err(NuError::TypeError {
                                msg: format!(
                                    "cannot assign to immutable binding `{}`; \
                                     use `var {} = ...` for a mutable local, \
                                     or `let {} = <new value> in ...` to shadow the binding.",
                                    name, name, name
                                ),
                                span: *span,
                                expected_type: Some("mutable binding".to_string()),
                                found_type: Some("immutable binding".to_string()),
                                similar_names: None,
                            });
                        }
                        // For field access, deref, etc., re-run mgu for its error
                        return Err(mgu(&target_ty_resolved, &expected_ref, *span).unwrap_err());
                    }
                };
                let final_subst = compose_subst(&s3, &compose_subst(&s2, &s1));
                Ok((final_subst, Type::unit()))
            }
            // Defer — just typecheck the deferred expression; the defer
            // itself has unit type.
            Expr::Defer { expr, .. } => {
                let _ = self.infer_expr(ctx, expr)?;
                Ok((vec![], Type::unit()))
            }
            // Panic diverges: a fresh type var unifies with any branch type.
            Expr::Panic(..) => Ok((vec![], Type::Var(TypeVar::fresh()))),
            Expr::Hide { names, body, .. } => {
                let mut scoped = ctx.clone();
                scoped.hide_names(names);
                self.infer_expr(&scoped, body)
            }
            Expr::Seal { names, body, .. } => {
                let mut scoped = ctx.clone();
                scoped.seal_except(names);
                self.infer_expr(&scoped, body)
            }
            Expr::Resume { .. } => Ok((vec![], Type::unit())),
        }
    }

    // -----------------------------------------------------------------------
    // Inference helpers for each expression form
    // -----------------------------------------------------------------------

    /// Infer the type of a literal.
    fn infer_literal(&mut self, lit: &Literal, _span: Span) -> NuResult<(Substitution, Type)> {
        let ty = match lit {
            Literal::Int(_) => Type::int(),
            Literal::Float(_) => Type::float(),
            Literal::String(_) => Type::string(),
            Literal::Bool(_) => Type::bool(),
            Literal::Nil => Type::nil(),
            Literal::Unit => Type::unit(),
        };
        Ok((vec![], ty))
    }

    /// Infer the type of a variable reference.
    fn infer_var(
        &mut self,
        ctx: &TypeContext,
        name: &str,
        span: Span,
    ) -> NuResult<(Substitution, Type)> {
        match ctx.lookup(name) {
            Some((ty, _cap, _mutable)) => {
                let instantiated = instantiate(ty);
                Ok((vec![], instantiated))
            }
            None => {
                let in_scope: Vec<String> = ctx.iter().map(|(n, _)| n.clone()).collect();
                Err(NuError::unbound_variable(name, span, Some(in_scope)))
            }
        }
    }

    /// Infer the type of a lambda expression.
    fn infer_lambda(
        &mut self,
        ctx: &TypeContext,
        params: &[crate::ast::Param],
        body: &Expr,
        effect: Option<&EffectRow>,
        _span: Span,
    ) -> NuResult<(Substitution, Type)> {
        let mut new_ctx = ctx.clone();
        let mut param_types = vec![];
        for p in params {
            let pty = match &p.ty {
                Some(t) => t.clone(),
                None => Type::Var(TypeVar::fresh()),
            };
            new_ctx.bind(p.name.clone(), pty.clone(), Capability::Ref, false);
            param_types.push(pty);
        }

        let (s, ret_ty) = self.infer_expr(&new_ctx, body)?;

        let param_ty = if param_types.len() == 1 {
            apply_subst(&param_types[0], &s)
        } else {
            Type::Tuple(param_types.iter().map(|t| apply_subst(t, &s)).collect())
        };

        let func_ty = Type::Function {
            param: Box::new(param_ty),
            ret: Box::new(apply_subst(&ret_ty, &s)),
            effect: effect.cloned().unwrap_or_else(EffectRow::empty),
            cap: Capability::Ref,
        };

        Ok((s, func_ty))
    }

    /// Infer the type of a function application.
    fn infer_app(
        &mut self,
        ctx: &TypeContext,
        func: &Expr,
        args: &[Expr],
        span: Span,
    ) -> NuResult<(Substitution, Type)> {
        let (s1, func_ty) = self.infer_expr(ctx, func)?;
        let mut subst = s1;
        let mut arg_types = vec![];

        // Infer each argument
        for arg in args {
            let ctx_sub = apply_subst_to_ctx(ctx, &subst);
            let (s_arg, arg_ty) = self.infer_expr(&ctx_sub, arg)?;
            subst = compose_subst(&s_arg, &subst);
            arg_types.push(apply_subst(&arg_ty, &subst));
        }

        // Resolve `using` params from `given` bindings.
        let mut extra_given_args: Vec<Expr> = Vec::new();
        if let Expr::Var(fn_name, _) = func {
            if let Some(using_names) = self.fn_using_params.get(fn_name) {
                for uname in using_names {
                    if let Some((_, val)) = self.given_bindings.get(uname) {
                        extra_given_args.push(val.clone());
                    }
                }
            }
        }
        for arg in &extra_given_args {
            let ctx_sub = apply_subst_to_ctx(ctx, &subst);
            let (s_arg, arg_ty) = self.infer_expr(&ctx_sub, arg)?;
            subst = compose_subst(&s_arg, &subst);
            arg_types.push(apply_subst(&arg_ty, &subst));
        }

        // Check arity if the function type is already resolved to a Function type
        let func_ty_subst = apply_subst(&func_ty, &subst);
        if let Type::Function {
            param: ref fn_param,
            ..
        } = &func_ty_subst
        {
            let expected_count = match fn_param.as_ref() {
                Type::Tuple(types) => types.len(),
                _ => 1,
            };
            if arg_types.len() != expected_count {
                // Allow a single tuple argument when the function expects
                // a tuple param (e.g. Make((1, 2)) for Make((T, T))).
                let single_tuple_ok = arg_types.len() == 1
                    && matches!(fn_param.as_ref(), Type::Tuple(_))
                    && matches!(&arg_types[0], Type::Tuple(_));
                if !single_tuple_ok {
                    let plural = |n: usize| if n == 1 { "argument" } else { "arguments" };
                    let expected_desc = format!("{} {}", expected_count, plural(expected_count));
                    let found_desc = format!("{} {}", arg_types.len(), plural(arg_types.len()));
                    return Err(NuError::TypeError {
                        msg: format!(
                            "wrong number of arguments: expected {}, got {}",
                            expected_desc, found_desc
                        ),
                        span,
                        expected_type: Some(expected_desc),
                        found_type: Some(found_desc),
                        similar_names: None,
                    });
                }
            }
        }

        // Create a fresh result type
        let result_ty = Type::Var(TypeVar::fresh());

        // Build expected function type
        let param_ty = if arg_types.len() == 1 {
            arg_types[0].clone()
        } else {
            Type::Tuple(arg_types)
        };

        // Preserve the function's effect row instead of forcing it to empty.
        // If the function type is not yet known, use a fresh open row so that
        // row-polymorphic functions can still unify.
        let func_ty_subst = apply_subst(&func_ty, &subst);
        let expected_effect = match &func_ty_subst {
            Type::Function { effect, .. } => effect.clone(),
            _ => EffectRow::Open(vec![], Region::fresh()),
        };

        let expected = Type::Function {
            param: Box::new(param_ty),
            ret: Box::new(result_ty.clone()),
            effect: expected_effect,
            cap: Capability::Ref,
        };

        // Unify
        let s2 = mgu(&func_ty_subst, &expected, span)?;
        let final_subst = compose_subst(&s2, &subst);

        Ok((final_subst.clone(), apply_subst(&result_ty, &final_subst)))
    }

    /// Infer the type of a let binding.
    fn infer_let(
        &mut self,
        ctx: &TypeContext,
        name: &str,
        ann: Option<&Type>,
        value: &Expr,
        body: &Expr,
        mutable: bool,
        _span: Span,
    ) -> NuResult<(Substitution, Type)> {
        // For let-bound lambdas that reference themselves (e.g.
        // `let fac = fn(n) ... fac(n-1) ... in ...`), make the binding name
        // available inside the lambda body with a fresh type variable.
        if matches!(value, Expr::Lambda { .. }) {
            let rec_var = Type::Var(TypeVar::fresh());
            let ctx_with_rec =
                ctx.extend(name.to_string(), rec_var.clone(), Capability::Ref, mutable);
            let (s1, val_ty) = self.infer_expr(&ctx_with_rec, value)?;
            let s2 = mgu(
                &apply_subst(&rec_var, &s1),
                &apply_subst(&val_ty, &s1),
                Span::default(),
            )?;
            let mut s_combined = compose_subst(&s2, &s1);
            // An explicit annotation must unify with the inferred value type.
            if let Some(ann_ty) = ann {
                let s_ann = mgu(&apply_subst(&val_ty, &s_combined), ann_ty, Span::default())?;
                s_combined = compose_subst(&s_ann, &s_combined);
            }
            let gen_ty = self.do_generalize(ctx, &apply_subst(&val_ty, &s_combined));
            let new_ctx = ctx.extend(name.to_string(), gen_ty, Capability::Ref, mutable);
            let (s3, body_ty) = self.infer_expr(&new_ctx, body)?;
            let final_subst = compose_subst(&s3, &s_combined);
            return Ok((final_subst.clone(), apply_subst(&body_ty, &final_subst)));
        }

        // Infer the binding value
        let (s1, val_ty) = self.infer_expr(ctx, value)?;

        // An explicit annotation must unify with the inferred value type.
        let s1 = if let Some(ann_ty) = ann {
            let s_ann = mgu(&apply_subst(&val_ty, &s1), ann_ty, Span::default())?;
            compose_subst(&s_ann, &s1)
        } else {
            s1
        };

        // Generalize the value type
        let gen_ty = self.do_generalize(ctx, &apply_subst(&val_ty, &s1));

        // Extend context with generalized type
        let new_ctx = ctx.extend(name.to_string(), gen_ty, Capability::Ref, mutable);

        // Infer body with extended context
        let (s2, body_ty) = self.infer_expr(&new_ctx, body)?;

        let final_subst = compose_subst(&s2, &s1);
        Ok((final_subst.clone(), apply_subst(&body_ty, &final_subst)))
    }

    /// Infer the type of a recursive let binding.
    fn infer_letrec(
        &mut self,
        ctx: &TypeContext,
        name: &str,
        params: &[crate::ast::Param],
        value: &Expr,
        body: &Expr,
        _span: Span,
    ) -> NuResult<(Substitution, Type)> {
        // Create a fresh type variable for the recursive function
        let rec_var = Type::Var(TypeVar::fresh());
        let ctx_with_rec = ctx.extend(name.to_string(), rec_var.clone(), Capability::Ref, false);

        // Infer the value with the recursive binding in scope
        let mut new_ctx = ctx_with_rec.clone();
        let mut param_types = vec![];
        for p in params {
            let pty = match &p.ty {
                Some(t) => t.clone(),
                None => Type::Var(TypeVar::fresh()),
            };
            new_ctx.bind(p.name.clone(), pty.clone(), Capability::Ref, false);
            param_types.push(pty);
        }

        let (s1, val_ty) = self.infer_expr(&new_ctx, value)?;

        // Build the function type from the value
        let func_ty = match &val_ty {
            Type::Function { .. } => val_ty.clone(),
            _ => {
                let param_ty = if param_types.len() == 1 {
                    param_types[0].clone()
                } else {
                    Type::Tuple(param_types)
                };
                Type::Function {
                    param: Box::new(param_ty),
                    ret: Box::new(val_ty.clone()),
                    effect: EffectRow::empty(),
                    cap: Capability::Ref,
                }
            }
        };

        // Unify the recursive variable with the inferred function type
        let s2 = mgu(
            &apply_subst(&rec_var, &s1),
            &apply_subst(&func_ty, &s1),
            Span::default(),
        )?;
        let s_combined = compose_subst(&s2, &s1);

        // Generalize
        let gen_ty = self.do_generalize(ctx, &apply_subst(&func_ty, &s_combined));
        let final_ctx = ctx.extend(name.to_string(), gen_ty, Capability::Ref, false);

        // Infer body
        let (s3, body_ty) = self.infer_expr(&final_ctx, body)?;
        let final_subst = compose_subst(&s3, &s_combined);

        Ok((final_subst.clone(), apply_subst(&body_ty, &final_subst)))
    }

    /// Infer the type of an if expression.
    fn infer_if(
        &mut self,
        ctx: &TypeContext,
        cond: &Expr,
        then_branch: &Expr,
        else_branch: Option<&Box<Expr>>,
        span: Span,
    ) -> NuResult<(Substitution, Type)> {
        // Infer condition - must be Bool
        let (s1, cond_ty) = self.infer_expr(ctx, cond)?;
        let s_cond = mgu(&apply_subst(&cond_ty, &s1), &Type::bool(), span)?;
        let s1 = compose_subst(&s_cond, &s1);

        // Infer then branch
        let ctx1 = apply_subst_to_ctx(ctx, &s1);
        let (s2, then_ty) = self.infer_expr(&ctx1, then_branch)?;
        let s2 = compose_subst(&s2, &s1);

        // Infer else branch or use Unit
        let (s3, else_ty) = match else_branch {
            Some(else_expr) => {
                let ctx2 = apply_subst_to_ctx(ctx, &s2);
                let (s3, else_ty) = self.infer_expr(&ctx2, else_expr)?;
                (compose_subst(&s3, &s2), else_ty)
            }
            None => (s2.clone(), Type::unit()),
        };

        // Unify then and else branches
        let s4 = mgu(
            &apply_subst(&then_ty, &s3),
            &apply_subst(&else_ty, &s3),
            span,
        )?;
        let final_subst = compose_subst(&s4, &s3);

        Ok((final_subst.clone(), apply_subst(&then_ty, &final_subst)))
    }

    /// Infer the type of a binary operator expression.
    fn infer_binary(
        &mut self,
        ctx: &TypeContext,
        op: BinOp,
        left: &Expr,
        right: &Expr,
        span: Span,
    ) -> NuResult<(Substitution, Type)> {
        use BinOp::*;

        match op {
            // Arithmetic: numeric -> numeric, except Add which also works on strings.
            Add => {
                let (s1, left_ty) = self.infer_expr(ctx, left)?;
                let ctx1 = apply_subst_to_ctx(ctx, &s1);
                let (s2, right_ty) = self.infer_expr(&ctx1, right)?;

                // If both sides are strings, this is concatenation.
                let s2s1 = compose_subst(&s2, &s1);
                let lty = apply_subst(&left_ty, &s2s1);
                let rty = apply_subst(&right_ty, &s2s1);
                if lty == Type::string() || rty == Type::string() {
                    return Ok((s2s1, Type::string()));
                }

                // Otherwise: numeric addition (same as Sub/Mul/Div/Mod).
                let num_var = Type::Var(TypeVar::fresh());
                let s3 = mgu(&apply_subst(&left_ty, &s1), &num_var, span)?;
                let s_combined = compose_subst(&s3, &compose_subst(&s2, &s1));
                let s4 = mgu(
                    &apply_subst(&right_ty, &s_combined),
                    &apply_subst(&num_var, &s_combined),
                    span,
                )?;
                let final_subst = compose_subst(&s4, &s_combined);
                Ok((final_subst.clone(), apply_subst(&num_var, &final_subst)))
            }

            // Other arithmetic: numeric -> numeric
            Sub | Mul | Div | Mod | Pow => {
                let (s1, left_ty) = self.infer_expr(ctx, left)?;
                let ctx1 = apply_subst_to_ctx(ctx, &s1);
                let (s2, right_ty) = self.infer_expr(&ctx1, right)?;

                let num_var = Type::Var(TypeVar::fresh());
                let s3 = mgu(&apply_subst(&left_ty, &s1), &num_var, span)?;
                let s_combined = compose_subst(&s3, &compose_subst(&s2, &s1));
                let s4 = mgu(
                    &apply_subst(&right_ty, &s_combined),
                    &apply_subst(&num_var, &s_combined),
                    span,
                )?;
                let final_subst = compose_subst(&s4, &s_combined);

                Ok((final_subst.clone(), apply_subst(&num_var, &final_subst)))
            }

            // Comparison: comparable -> Bool
            Eq | Ne | Lt | Le | Gt | Ge => {
                let (s1, left_ty) = self.infer_expr(ctx, left)?;
                let ctx1 = apply_subst_to_ctx(ctx, &s1);
                let (s2, right_ty) = self.infer_expr(&ctx1, right)?;

                let combined = compose_subst(&s2, &s1);
                let lty = apply_subst(&left_ty, &combined);
                let rty = apply_subst(&right_ty, &combined);

                // Nil is a universal sentinel: == nil and != nil are valid
                // for any type (including perform results whose type may
                // unify with Nil but whose runtime value may be non-nil).
                if matches!(op, Eq | Ne) && (lty == Type::nil() || rty == Type::nil()) {
                    return Ok((combined, Type::bool()));
                }

                let s3 = mgu(&rty, &lty, span)?;
                let final_subst = compose_subst(&s3, &combined);

                Ok((final_subst, Type::bool()))
            }

            // Boolean: Bool -> Bool
            And | Or => {
                let (s1, left_ty) = self.infer_expr(ctx, left)?;
                let s_left = mgu(&left_ty, &Type::bool(), span)?;
                let s1 = compose_subst(&s_left, &s1);

                let ctx1 = apply_subst_to_ctx(ctx, &s1);
                let (s2, right_ty) = self.infer_expr(&ctx1, right)?;
                let combined = compose_subst(&s2, &s1);
                let s_right = mgu(&apply_subst(&right_ty, &combined), &Type::bool(), span)?;
                let final_subst = compose_subst(&s_right, &combined);

                Ok((final_subst, Type::bool()))
            }

            // Bitwise: Int -> Int
            BitAnd | BitOr | BitXor | Shl | Shr => {
                let (s1, left_ty) = self.infer_expr(ctx, left)?;
                let s_left = mgu(&left_ty, &Type::int(), span)?;
                let s1 = compose_subst(&s_left, &s1);

                let ctx1 = apply_subst_to_ctx(ctx, &s1);
                let (s2, right_ty) = self.infer_expr(&ctx1, right)?;
                let combined = compose_subst(&s2, &s1);
                let s_right = mgu(&apply_subst(&right_ty, &combined), &Type::int(), span)?;
                let final_subst = compose_subst(&s_right, &combined);

                Ok((final_subst, Type::int()))
            }

            // Assignment (should be handled in Assign expr, but here for completeness)
            Assign => {
                let (s1, left_ty) = self.infer_expr(ctx, left)?;
                let ctx1 = apply_subst_to_ctx(ctx, &s1);
                let (s2, right_ty) = self.infer_expr(&ctx1, right)?;
                let s3 = mgu(
                    &apply_subst(&right_ty, &s2),
                    &apply_subst(&left_ty, &s1),
                    span,
                )?;
                let final_subst = compose_subst(&s3, &compose_subst(&s2, &s1));
                Ok((final_subst, Type::unit()))
            }

            // Range: Int -> Int -> Array[Int]
            Range => {
                let (s1, left_ty) = self.infer_expr(ctx, left)?;
                let s_left = mgu(&left_ty, &Type::int(), span)?;
                let s1 = compose_subst(&s_left, &s1);

                let ctx1 = apply_subst_to_ctx(ctx, &s1);
                let (s2, right_ty) = self.infer_expr(&ctx1, right)?;
                let combined = compose_subst(&s2, &s1);
                let s_right = mgu(&apply_subst(&right_ty, &combined), &Type::int(), span)?;
                let final_subst = compose_subst(&s_right, &combined);

                Ok((final_subst, Type::Array(Box::new(Type::int()))))
            }

            // Pipe (should be handled in Pipe expr)
            Pipe => {
                let (s1, left_ty) = self.infer_expr(ctx, left)?;
                let ctx1 = apply_subst_to_ctx(ctx, &s1);
                let (s2, right_ty) = self.infer_expr(&ctx1, right)?;
                let result_var = Type::Var(TypeVar::fresh());
                let expected = Type::Function {
                    param: Box::new(apply_subst(&left_ty, &compose_subst(&s2, &s1))),
                    ret: Box::new(result_var.clone()),
                    effect: EffectRow::empty(),
                    cap: Capability::Ref,
                };
                let s3 = mgu(&apply_subst(&right_ty, &s2), &expected, span)?;
                let final_subst = compose_subst(&s3, &compose_subst(&s2, &s1));
                Ok((final_subst.clone(), apply_subst(&result_var, &final_subst)))
            }
        }
    }

    /// Infer the type of a unary operator expression.
    fn infer_unary(
        &mut self,
        ctx: &TypeContext,
        op: UnOp,
        expr: &Expr,
        span: Span,
    ) -> NuResult<(Substitution, Type)> {
        use UnOp::*;

        let (s, ty) = self.infer_expr(ctx, expr)?;

        match op {
            // Negation: numeric -> numeric
            Neg => {
                let num_var = Type::Var(TypeVar::fresh());
                let s2 = mgu(&apply_subst(&ty, &s), &num_var, span)?;
                let final_subst = compose_subst(&s2, &s);
                Ok((final_subst.clone(), apply_subst(&num_var, &final_subst)))
            }
            // Boolean not: Bool -> Bool
            Not => {
                let s2 = mgu(&apply_subst(&ty, &s), &Type::bool(), span)?;
                let final_subst = compose_subst(&s2, &s);
                Ok((final_subst, Type::bool()))
            }
            // Dereference: *e where e : &cap T  =>  T
            // Works for any reference capability (ref, val, iso, etc.).
            Deref => {
                let resolved = apply_subst(&ty, &s);
                match resolved {
                    Type::Reference { inner, .. } => {
                        // Already resolved to a reference: peel it.
                        Ok((s, *inner))
                    }
                    Type::Var(_) => {
                        // Still a type variable: constrain it to be a
                        // reference.  Default to Ref for bare *x without
                        // context; the outer mgu will refine as needed.
                        let inner_var = Type::Var(TypeVar::fresh());
                        let s2 = mgu(
                            &resolved,
                            &Type::Reference {
                                cap: Capability::Ref,
                                inner: Box::new(inner_var.clone()),
                            },
                            span,
                        )?;
                        let final_subst = compose_subst(&s2, &s);
                        Ok((final_subst.clone(), apply_subst(&inner_var, &final_subst)))
                    }
                    other => Err(NuError::type_mismatch(
                        format!("cannot dereference type `{}`", other),
                        "reference type".to_string(),
                        span,
                    )),
                }
            }
            // Reference: T -> &cap T
            Ref(cap) => {
                let ref_ty = Type::Reference {
                    cap,
                    inner: Box::new(apply_subst(&ty, &s)),
                };
                Ok((s, ref_ty))
            }
        }
    }

    /// Infer the type of a tuple expression.
    fn infer_tuple(
        &mut self,
        ctx: &TypeContext,
        exprs: &[Expr],
        _span: Span,
    ) -> NuResult<(Substitution, Type)> {
        let mut subst = vec![];
        let mut types = vec![];
        for expr in exprs {
            let ctx_sub = apply_subst_to_ctx(ctx, &subst);
            let (s, ty) = self.infer_expr(&ctx_sub, expr)?;
            subst = compose_subst(&s, &subst);
            types.push(apply_subst(&ty, &subst));
        }
        Ok((subst, Type::Tuple(types)))
    }

    /// Infer the type of a record expression.
    fn infer_record(
        &mut self,
        ctx: &TypeContext,
        fields: &[(String, Expr)],
        _span: Span,
    ) -> NuResult<(Substitution, Type)> {
        let mut subst = vec![];
        let mut field_types = vec![];
        for (name, expr) in fields {
            let ctx_sub = apply_subst_to_ctx(ctx, &subst);
            let (s, ty) = self.infer_expr(&ctx_sub, expr)?;
            subst = compose_subst(&s, &subst);
            field_types.push((name.clone(), apply_subst(&ty, &subst)));
        }
        Ok((subst, Type::Record(field_types)))
    }

    /// Infer the type of a record-update expression: { base .. field = val, ... }
    fn infer_record_update(
        &mut self,
        ctx: &TypeContext,
        base: &Expr,
        overrides: &[(String, Expr)],
        span: Span,
    ) -> NuResult<(Substitution, Type)> {
        // Infer the base type — it must be a record.
        let (s_base, base_ty) = self.infer_expr(ctx, base)?;
        let base_ty = apply_subst(&base_ty, &s_base);
        let base_fields = match &base_ty {
            Type::Record(fs) => fs.clone(),
            Type::Var(_) => {
                // If the base type is a type variable, it's an unbound record —
                // this happens if the base comes from a polymorphic function.
                // We'll still type-check the overrides and return a fresh record type.
                return Err(NuError::type_mismatch(
                    format!("{}", Type::Record(vec![])),
                    format!("{}", base_ty),
                    span,
                ));
            }
            other => {
                return Err(NuError::type_mismatch(
                    format!("{}", Type::Record(vec![])),
                    format!("{}", other),
                    span,
                ));
            }
        };

        // Check each override field exists in the base record type and
        // unify the override value type with the field's type.
        let mut subst = s_base;
        let ctx = apply_subst_to_ctx(ctx, &subst);

        for (field_name, override_expr) in overrides {
            let base_field_ty = base_fields
                .iter()
                .find(|(n, _)| n == field_name)
                .map(|(_, t)| t.clone());

            match base_field_ty {
                Some(expected_ty) => {
                    let (s_ov, ov_ty) = self.infer_expr(&ctx, override_expr)?;
                    subst = compose_subst(&s_ov, &subst);
                    let expected = apply_subst(&expected_ty, &subst);
                    let ov_ty = apply_subst(&ov_ty, &subst);
                    let s_unify = mgu(&expected, &ov_ty, span)?;
                    subst = compose_subst(&s_unify, &subst);
                }
                None => {
                    let available: Vec<String> =
                        base_fields.iter().map(|(n, _)| n.clone()).collect();
                    return Err(NuError::field_not_found(field_name, span, Some(available)));
                }
            }
        }

        let result_ty = apply_subst(&base_ty, &subst);
        Ok((subst, result_ty))
    }

    /// Infer the type of a field access expression.
    fn infer_field_access(
        &mut self,
        ctx: &TypeContext,
        expr: &Expr,
        field: &str,
        span: Span,
    ) -> NuResult<(Substitution, Type)> {
        // Pipeline built-in namespace: Pipeline.new / Pipeline.stage.
        if let Expr::Var(base, _) = expr {
            if base == "Pipeline" {
                if field == "new" {
                    let func_ty = Type::Function {
                        param: Box::new(Type::Tuple(vec![])),
                        ret: Box::new(Type::int()),
                        effect: EffectRow::empty(),
                        cap: Capability::Ref,
                    };
                    return Ok((vec![], func_ty));
                }
                if field == "stage" {
                    let actor_ty = Type::Actor {
                        state: Box::new(Type::Var(TypeVar::fresh())),
                        behavior: Box::new(Type::Var(TypeVar::fresh())),
                    };
                    let func_ty = Type::Function {
                        param: Box::new(Type::Tuple(vec![
                            Type::int(),
                            Type::string(),
                            actor_ty,
                            Type::string(),
                        ])),
                        ret: Box::new(Type::int()),
                        effect: EffectRow::empty(),
                        cap: Capability::Ref,
                    };
                    return Ok((vec![], func_ty));
                }
            }
            // Supervisor built-in namespace: Supervisor.new / Supervisor.worker.
            if base == "Supervisor" {
                if field == "new" {
                    let func_ty = Type::Function {
                        param: Box::new(Type::Tuple(vec![])),
                        ret: Box::new(Type::int()),
                        effect: EffectRow::empty(),
                        cap: Capability::Ref,
                    };
                    return Ok((vec![], func_ty));
                }
                if field == "worker" {
                    let actor_ty = Type::Actor {
                        state: Box::new(Type::Var(TypeVar::fresh())),
                        behavior: Box::new(Type::Var(TypeVar::fresh())),
                    };
                    let func_ty = Type::Function {
                        param: Box::new(Type::Tuple(vec![
                            Type::int(),
                            Type::string(),
                            actor_ty,
                            Type::string(),
                        ])),
                        ret: Box::new(Type::int()),
                        effect: EffectRow::empty(),
                        cap: Capability::Ref,
                    };
                    return Ok((vec![], func_ty));
                }
            }
            // Debate built-in namespace: Debate.new / Debate.participant.
            if base == "Debate" {
                if field == "new" {
                    let func_ty = Type::Function {
                        param: Box::new(Type::Tuple(vec![
                            Type::string(),
                            Type::int(),
                            Type::float(),
                        ])),
                        ret: Box::new(Type::int()),
                        effect: EffectRow::empty(),
                        cap: Capability::Ref,
                    };
                    return Ok((vec![], func_ty));
                }
                if field == "participant" {
                    let actor_ty = Type::Actor {
                        state: Box::new(Type::Var(TypeVar::fresh())),
                        behavior: Box::new(Type::Var(TypeVar::fresh())),
                    };
                    let func_ty = Type::Function {
                        param: Box::new(Type::Tuple(vec![
                            Type::int(),
                            Type::string(),
                            Type::string(),
                            actor_ty,
                        ])),
                        ret: Box::new(Type::int()),
                        effect: EffectRow::empty(),
                        cap: Capability::Ref,
                    };
                    return Ok((vec![], func_ty));
                }
            }
        }

        // Pipeline / Supervisor / Debate instance method: <id>.run(...)
        if field == "run" {
            let (s1, receiver_ty) = self.infer_expr(ctx, expr)?;
            let s_receiver = mgu(&apply_subst(&receiver_ty, &s1), &Type::int(), span)?;
            let final_subst = compose_subst(&s_receiver, &s1);
            // Debate.run() takes no arguments; Pipeline/Supervisor.run() take a
            // single string input.  We use the receiver variable name as a v0.9
            // MVP heuristic, matching the compiler disambiguation.
            let param_ty = if let Expr::Var(receiver_name, _) = expr {
                let lowered = receiver_name.to_lowercase();
                if lowered == "debate" || lowered.contains("debate") {
                    Type::Tuple(vec![])
                } else {
                    Type::string()
                }
            } else {
                Type::string()
            };
            let func_ty = Type::Function {
                param: Box::new(param_ty),
                ret: Box::new(Type::string()),
                effect: EffectRow::empty(),
                cap: Capability::Ref,
            };
            return Ok((final_subst, func_ty));
        }

        let (s1, record_ty) = self.infer_expr(ctx, expr)?;
        let mut current_ty = apply_subst(&record_ty, &s1);

        // Peel reference wrappers so field access works through
        // &ref / &val / &iso etc.  x.field where x : &ref {field: T} => T.
        loop {
            match current_ty {
                Type::Reference { inner, .. } => {
                    current_ty = apply_subst(&inner, &s1);
                }
                _ => break,
            }
        }

        match current_ty {
            Type::Record(ref fs) => {
                let (fields, tail) = split_record(fs);
                if let Some((_, field_ty)) = fields.iter().find(|(name, _)| name == field) {
                    return Ok((s1, field_ty.clone()));
                }
                // An open record absorbs the access: extend its row variable
                // with the demanded field, so multiple accesses on the same
                // record accumulate row-polymorphically.
                if let Some(Type::Var(row)) = tail {
                    let field_var = Type::Var(TypeVar::fresh());
                    let extension = Type::record_open(
                        vec![(field.to_string(), field_var.clone())],
                        TypeVar::fresh(),
                    );
                    let s2 = mgu(&Type::Var(row), &extension, span)?;
                    return Ok((compose_subst(&s2, &s1), field_var));
                }
                let available: Vec<String> = fields.iter().map(|(n, _)| n.clone()).collect();
                Err(NuError::field_not_found(field, span, Some(available)))
            }
            Type::Tuple(ref elems) => {
                // Numeric field access on a tuple: t.0, t.1, etc.
                if let Ok(idx) = field.parse::<usize>() {
                    if idx < elems.len() {
                        return Ok((s1, elems[idx].clone()));
                    }
                    return Err(NuError::TypeError {
                        msg: format!(
                            "tuple index {} out of range for tuple of length {}",
                            idx,
                            elems.len()
                        ),
                        span,
                        expected_type: None,
                        found_type: None,
                        similar_names: None,
                    });
                }
                // Tuple with non-numeric field access: error.
                Err(NuError::TypeError {
                    msg: format!("tuple type does not have field '{}'", field),
                    span,
                    expected_type: None,
                    found_type: None,
                    similar_names: None,
                })
            }
            _ => {
                // Typeclass method resolution: if the receiver type is
                // concrete and there is a matching instance for a class
                // that defines `field`, resolve through the instance
                // dictionary. The returned type drops the `self` parameter
                // so `App` can apply the remaining arguments naturally.
                let concrete = !matches!(&current_ty, Type::Var(_) | Type::Skolem(_));
                if concrete {
                    let type_key = format!("{}", current_ty);
                    let class_table = self.class_table.clone();
                    let instance_table = self.instance_table.clone();
                    for (class_name, class_info) in &class_table {
                        if class_info.methods.iter().any(|m| m.name == field) {
                            let key = (class_name.clone(), type_key.clone());
                            if instance_table.contains_key(&key) {
                                let dict_name = format!("_impl_{}_{}", class_name, type_key);
                                if let Some((dict_ty, _, _)) = ctx.lookup(&dict_name) {
                                    if let Type::Record(fields) = dict_ty {
                                        if let Some((_, field_ty)) =
                                            fields.iter().find(|(n, _)| n == field)
                                        {
                                            if let Type::Function {
                                                param,
                                                ret,
                                                effect,
                                                cap,
                                            } = field_ty
                                            {
                                                let remaining = strip_first_param(param);
                                                return Ok((
                                                    s1.clone(),
                                                    Type::Function {
                                                        param: Box::new(remaining),
                                                        ret: ret.clone(),
                                                        effect: effect.clone(),
                                                        cap: *cap,
                                                    },
                                                ));
                                            }
                                        }
                                    }
                                }
                                // Instance registered but dict not in
                                // context (should not happen).
                                return Err(NuError::TypeError {
                                    msg: format!("internal: dict '{}' not in scope", dict_name),
                                    span,
                                    expected_type: None,
                                    found_type: None,
                                    similar_names: None,
                                });
                            }
                            return Err(NuError::TypeError {
                                msg: format!("no impl {}[{}]", class_name, current_ty),
                                span,
                                expected_type: None,
                                found_type: None,
                                similar_names: None,
                            });
                        }
                    }
                }

                // If the receiver is a type variable with class constraints,
                // resolve the method through the constrained class's declaration.
                // Also handles skolemized type parameters (their skolem ID is
                // used as a TypeVar key in the constraint map).
                let constraint_key = match &current_ty {
                    Type::Var(tv) => Some(*tv),
                    Type::Skolem(id) => Some(TypeVar(*id)),
                    _ => None,
                };
                if let Some(tv) = constraint_key {
                    if let Some(class_names) = ctx.get_constraints(&tv) {
                        for class_name in class_names {
                            if let Some(class_info) = self.class_table.get(class_name) {
                                if let Some(method) =
                                    class_info.methods.iter().find(|m| m.name == field)
                                {
                                    // Build method type from the class declaration.
                                    let remaining_params: Vec<Type> = method.params[1..]
                                        .iter()
                                        .map(|_| Type::Var(TypeVar::fresh()))
                                        .collect();
                                    let remaining = if remaining_params.len() == 1 {
                                        remaining_params[0].clone()
                                    } else if remaining_params.is_empty() {
                                        Type::unit()
                                    } else {
                                        Type::Tuple(remaining_params)
                                    };
                                    let func_ty = Type::Function {
                                        param: Box::new(remaining),
                                        ret: Box::new(method.return_type.clone()),
                                        effect: EffectRow::empty(),
                                        cap: Capability::Ref,
                                    };
                                    return Ok((s1.clone(), func_ty));
                                }
                            }
                        }
                    }
                }
                // If the receiver is a concrete type that is not a record,
                // not a tuple, and no class method matched, the user is
                // likely trying method-call syntax on a built-in type.
                // Produce a clear error instead of the confusing
                if !matches!(&current_ty, Type::Var(_) | Type::Skolem(_)) {
                    return Err(NuError::parse_error(
                        format!(
                            "method-call syntax (`.{}()`) is not supported for type `{}`; use `perform <Effect>.op(args)` for built-in operations",
                            field, current_ty
                        ),
                        span,
                    ));
                }
                // Unknown receiver shape or no class defines this method:
                // require an open record carrying the demanded field,
                // leaving the rest of the row to be inferred from other
                // accesses or the call site.
                let field_var = Type::Var(TypeVar::fresh());
                let expected = Type::record_open(
                    vec![(field.to_string(), field_var.clone())],
                    TypeVar::fresh(),
                );
                let s2 = mgu(&current_ty, &expected, span)?;
                let final_subst = compose_subst(&s2, &s1);
                Ok((final_subst.clone(), apply_subst(&field_var, &final_subst)))
            }
        }
    }

    /// Infer the type of an array expression.
    fn infer_array(
        &mut self,
        ctx: &TypeContext,
        elems: &[Expr],
        _span: Span,
    ) -> NuResult<(Substitution, Type)> {
        if elems.is_empty() {
            let elem_var = Type::Var(TypeVar::fresh());
            return Ok((vec![], Type::Array(Box::new(elem_var))));
        }

        let mut subst = vec![];
        let (s1, first_ty) = self.infer_expr(ctx, &elems[0])?;
        subst = s1;

        for elem in &elems[1..] {
            let ctx_sub = apply_subst_to_ctx(ctx, &subst);
            let (s, ty) = self.infer_expr(&ctx_sub, elem)?;
            let s_unify = mgu(
                &apply_subst(&ty, &s),
                &apply_subst(&first_ty, &subst),
                Span::default(),
            )?;
            subst = compose_subst(&s_unify, &compose_subst(&s, &subst));
        }

        Ok((
            subst.clone(),
            Type::Array(Box::new(apply_subst(&first_ty, &subst))),
        ))
    }

    /// Infer the type of an array index expression.
    fn infer_index(
        &mut self,
        ctx: &TypeContext,
        arr: &Expr,
        idx: &Expr,
        _span: Span,
    ) -> NuResult<(Substitution, Type)> {
        let (s1, arr_ty) = self.infer_expr(ctx, arr)?;
        let ctx1 = apply_subst_to_ctx(ctx, &s1);
        let (s2, idx_ty) = self.infer_expr(&ctx1, idx)?;

        // Index must be Int
        let s_idx = mgu(&apply_subst(&idx_ty, &s2), &Type::int(), Span::default())?;
        let s_combined = compose_subst(&s_idx, &compose_subst(&s2, &s1));

        // Array type
        let elem_var = Type::Var(TypeVar::fresh());
        let s_arr = mgu(
            &apply_subst(&arr_ty, &s_combined),
            &Type::Array(Box::new(elem_var.clone())),
            Span::default(),
        )?;
        let final_subst = compose_subst(&s_arr, &s_combined);

        Ok((final_subst.clone(), apply_subst(&elem_var, &final_subst)))
    }

    /// Infer the type of a pattern match expression.
    fn infer_match(
        &mut self,
        ctx: &TypeContext,
        scrutinee: &Expr,
        arms: &[(Pattern, Option<Expr>, Expr)],
        span: Span,
    ) -> NuResult<(Substitution, Type)> {
        // Infer scrutinee type
        let (s1, scrut_ty) = self.infer_expr(ctx, scrutinee)?;

        // Infer each arm
        let mut subst = s1;
        let mut arm_types = vec![];

        for (pattern, guard, arm_expr) in arms {
            let ctx_sub = apply_subst_to_ctx(ctx, &subst);
            // Bind pattern variables to the context
            let pattern_ctx =
                self.bind_pattern(&ctx_sub, pattern, &apply_subst(&scrut_ty, &subst))?;
            // A guard runs with the pattern's bindings in scope and must be
            // a boolean; it does not contribute to the arm result type.
            if let Some(guard_expr) = guard {
                let (s_guard, guard_ty) = self.infer_expr(&pattern_ctx, guard_expr)?;
                let guard_subst = compose_subst(&s_guard, &subst);
                let s_bool = mgu(&apply_subst(&guard_ty, &guard_subst), &Type::bool(), span)?;
                subst = compose_subst(&s_bool, &guard_subst);
            }
            let pattern_ctx = apply_subst_to_ctx(&pattern_ctx, &subst);
            let (s_arm, arm_ty) = self.infer_expr(&pattern_ctx, arm_expr)?;
            subst = compose_subst(&s_arm, &subst);
            arm_types.push(arm_ty);
        }
        if arm_types.is_empty() {
            return Err(NuError::TypeError {
                msg: "Match expression with no arms".to_string(),
                span,
                expected_type: Some("at least one match arm".to_string()),
                found_type: Some("0 match arms".to_string()),
                similar_names: None,
            });
        }

        // Unify all arm types
        let first_arm = arm_types[0].clone();
        let mut final_subst = subst;
        for arm_ty in &arm_types[1..] {
            let s = mgu(
                &apply_subst(arm_ty, &final_subst),
                &apply_subst(&first_arm, &final_subst),
                span,
            )?;
            final_subst = compose_subst(&s, &final_subst);
        }

        Ok((final_subst.clone(), apply_subst(&first_arm, &final_subst)))
    }

    /// Bind pattern variables into a new context.
    fn bind_pattern(
        &mut self,
        ctx: &TypeContext,
        pattern: &Pattern,
        scrut_ty: &Type,
    ) -> NuResult<TypeContext> {
        match pattern {
            Pattern::Wild => Ok(ctx.clone()),
            Pattern::Var(name) => {
                Ok(ctx.extend(name.clone(), scrut_ty.clone(), Capability::Ref, false))
            }
            Pattern::Lit(lit) => {
                let lit_ty = match lit {
                    Literal::Int(_) => Type::int(),
                    Literal::Float(_) => Type::float(),
                    Literal::String(_) => Type::string(),
                    Literal::Bool(_) => Type::bool(),
                    Literal::Nil => Type::nil(),
                    Literal::Unit => Type::unit(),
                };
                let _ = mgu(scrut_ty, &lit_ty, Span::default())?;
                Ok(ctx.clone())
            }
            Pattern::Tuple(pats) => {
                match scrut_ty {
                    Type::Tuple(tys) if tys.len() == pats.len() => {
                        let mut new_ctx = ctx.clone();
                        for (pat, ty) in pats.iter().zip(tys.iter()) {
                            new_ctx = self.bind_pattern(&new_ctx, pat, ty)?;
                        }
                        Ok(new_ctx)
                    }
                    _ => {
                        // Create fresh type vars for tuple elements
                        let mut new_ctx = ctx.clone();
                        for pat in pats {
                            let elem_ty = Type::Var(TypeVar::fresh());
                            new_ctx = self.bind_pattern(&new_ctx, pat, &elem_ty)?;
                        }
                        Ok(new_ctx)
                    }
                }
            }
            Pattern::Record(pats) => match scrut_ty {
                Type::Record(fields) => {
                    let mut new_ctx = ctx.clone();
                    let field_map: FxHashMap<String, Type> =
                        fields.iter().map(|(n, t)| (n.clone(), t.clone())).collect();
                    for (field_name, pat) in pats {
                        if let Some(ty) = field_map.get(field_name) {
                            new_ctx = self.bind_pattern(&new_ctx, pat, ty)?;
                        } else {
                            let fresh = Type::Var(TypeVar::fresh());
                            new_ctx = self.bind_pattern(&new_ctx, pat, &fresh)?;
                        }
                    }
                    Ok(new_ctx)
                }
                _ => {
                    let mut new_ctx = ctx.clone();
                    for (_, pat) in pats {
                        let fresh = Type::Var(TypeVar::fresh());
                        new_ctx = self.bind_pattern(&new_ctx, pat, &fresh)?;
                    }
                    Ok(new_ctx)
                }
            },
            Pattern::Variant(name, pat) => match scrut_ty {
                Type::Variant(variants) => {
                    let mut new_ctx = ctx.clone();
                    if let Some((_, Some(ty))) = variants.iter().find(|(n, _)| n == name) {
                        if let Some(p) = pat {
                            new_ctx = self.bind_pattern(&new_ctx, p, ty)?;
                        }
                    }
                    Ok(new_ctx)
                }
                _ => {
                    if let Some(p) = pat {
                        let fresh = Type::Var(TypeVar::fresh());
                        self.bind_pattern(ctx, p, &fresh)
                    } else {
                        Ok(ctx.clone())
                    }
                }
            },
            Pattern::Alias(name, pat) => {
                let mut new_ctx =
                    ctx.extend(name.clone(), scrut_ty.clone(), Capability::Ref, false);
                new_ctx = self.bind_pattern(&new_ctx, pat, scrut_ty)?;
                Ok(new_ctx)
            }
        }
    }

    fn infer_block(
        &mut self,
        ctx: &TypeContext,
        exprs: &[Expr],
        _span: Span,
    ) -> NuResult<(Substitution, Type)> {
        if exprs.is_empty() {
            return Ok((vec![], Type::unit()));
        }

        let mut subst = vec![];
        let mut last_ty = Type::unit();
        for expr in exprs {
            let ctx_sub = apply_subst_to_ctx(ctx, &subst);
            let (s, ty) = self.infer_expr(&ctx_sub, expr)?;
            subst = compose_subst(&s, &subst);
            last_ty = ty;
        }
        Ok((subst.clone(), apply_subst(&last_ty, &subst)))
    }

    fn infer_actor_decl(
        &mut self,
        ctx: &TypeContext,
        name: &str,
        behaviors: &[Behavior],
        events: &[crate::ast::EventDecl],
        migrations: &[crate::ast::MigrationDecl],
        _span: Span,
    ) -> NuResult<(Substitution, Type)> {
        // The actor's own name must be in scope inside its behaviors so an
        // actor can `spawn`/`send`/`ask` its own type (recursive actor graphs,
        // e.g. skynet). A placeholder `Type::Actor` suffices: spawn/send/ask
        // only require "is an actor", not the concrete state/behavior types.
        let self_ty = Type::Actor {
            state: Box::new(Type::Var(TypeVar::fresh())),
            behavior: Box::new(Type::Var(TypeVar::fresh())),
        };
        // Check each behavior, with event declarations in scope for emit checking
        for behavior in behaviors {
            let mut behavior_ctx = ctx.clone();
            behavior_ctx.bind(name.to_string(), self_ty.clone(), Capability::Ref, false);
            let mut param_types = vec![];
            for p in &behavior.params {
                let pty = match &p.ty {
                    Some(t) => t.clone(),
                    None => Type::Var(TypeVar::fresh()),
                };
                behavior_ctx.bind(
                    p.name.clone(),
                    pty.clone(),
                    p.cap.unwrap_or(Capability::Ref),
                    false,
                );
                param_types.push(pty);
            }
            if !events.is_empty() {
                let ctx_events: Vec<(String, Vec<(String, Type)>)> = events
                    .iter()
                    .map(|e| (e.name.clone(), e.params.clone()))
                    .collect();
                behavior_ctx.set_entity_events(ctx_events);
            }
            let (_s, _body_ty) = self.infer_expr(&behavior_ctx, &behavior.body)?;
        }

        // Typecheck migration contracts: state_body and event_migration handlers
        for migration in migrations {
            if let Some(ref state_body) = migration.state_body {
                let _ = self.infer_expr(ctx, state_body)?;
            }
            for (_ev_name, _ev_params, ev_body) in &migration.event_migrations {
                let _ = self.infer_expr(ctx, ev_body)?;
            }
        }

        let actor_ty = Type::Actor {
            state: Box::new(Type::Var(TypeVar::fresh())),
            behavior: Box::new(Type::Var(TypeVar::fresh())),
        };
        Ok((vec![], actor_ty))
    }

    /// Infer spawn expression.
    fn infer_spawn(
        &mut self,
        ctx: &TypeContext,
        actor_type: &Expr,
        target_node: Option<&Expr>,
        span: Span,
    ) -> NuResult<(Substitution, Type)> {
        let (s, actor_ty) = self.infer_expr(ctx, actor_type)?;
        let mut subst = s;
        if let Some(node) = target_node {
            let ctx_sub = apply_subst_to_ctx(ctx, &subst);
            let (s_node, _ty) = self.infer_expr(&ctx_sub, node)?;
            subst = compose_subst(&s_node, &subst);
        }
        match &actor_ty {
            Type::Actor { .. } => Ok((subst, actor_ty.clone())),
            _ => {
                // Try to unify with Actor type
                let fresh_actor = Type::Actor {
                    state: Box::new(Type::Var(TypeVar::fresh())),
                    behavior: Box::new(Type::Var(TypeVar::fresh())),
                };
                let s2 = mgu(&actor_ty, &fresh_actor, span)?;
                let final_subst = compose_subst(&s2, &subst);
                Ok((final_subst, fresh_actor))
            }
        }
    }

    /// Infer send expression.
    fn infer_send(
        &mut self,
        ctx: &TypeContext,
        actor: &Expr,
        _behavior: &str,
        args: &[Expr],
        span: Span,
    ) -> NuResult<(Substitution, Type)> {
        let (s1, actor_ty) = self.infer_expr(ctx, actor)?;

        // Actor must be an actor type
        let actor_var = TypeVar::fresh();
        let fresh_actor = Type::Actor {
            state: Box::new(Type::Var(actor_var)),
            behavior: Box::new(Type::Var(TypeVar::fresh())),
        };
        let s2 = mgu(&apply_subst(&actor_ty, &s1), &fresh_actor, span)?;
        let s_combined = compose_subst(&s2, &s1);

        // Infer argument types
        let mut subst = s_combined;
        for arg in args {
            let ctx_sub = apply_subst_to_ctx(ctx, &subst);
            let (s_arg, _arg_ty) = self.infer_expr(&ctx_sub, arg)?;
            subst = compose_subst(&s_arg, &subst);
        }

        // Send returns Unit
        Ok((subst, Type::unit()))
    }

    /// Infer ask expression.
    fn infer_ask(
        &mut self,
        ctx: &TypeContext,
        actor: &Expr,
        span: Span,
    ) -> NuResult<(Substitution, Type)> {
        let (s1, actor_ty) = self.infer_expr(ctx, actor)?;

        let fresh_actor = Type::Actor {
            state: Box::new(Type::Var(TypeVar::fresh())),
            behavior: Box::new(Type::Var(TypeVar::fresh())),
        };
        let s2 = mgu(&actor_ty, &fresh_actor, span)?;
        let subst = compose_subst(&s2, &s1);

        // Ask returns a fresh type (the behavior's return type)
        let ret_var = Type::Var(TypeVar::fresh());
        Ok((subst, ret_var))
    }

    /// Infer perform expression.
    fn infer_perform(
        &mut self,
        ctx: &TypeContext,
        _effect: &str,
        args: &[Expr],
        _span: Span,
    ) -> NuResult<(Substitution, Type)> {
        let mut subst = vec![];
        for arg in args {
            let ctx_sub = apply_subst_to_ctx(ctx, &subst);
            let (s, _ty) = self.infer_expr(&ctx_sub, arg)?;
            subst = compose_subst(&s, &subst);
        }
        // Perform returns a fresh type variable
        let ret_var = Type::Var(TypeVar::fresh());
        Ok((subst, ret_var))
    }

    /// Infer handle expression.
    fn infer_handle(
        &mut self,
        ctx: &TypeContext,
        body: &Expr,
        handlers: &[EffectHandler],
        _span: Span,
    ) -> NuResult<(Substitution, Type)> {
        // Infer body type
        let (mut subst, body_ty) = self.infer_expr(ctx, body)?;

        // Each handler body must produce a value compatible with the body's type.
        for h in handlers {
            let mut handler_ctx = apply_subst_to_ctx(ctx, &subst);
            for p in &h.params {
                handler_ctx.bind(
                    p.clone(),
                    Type::Var(TypeVar::fresh()),
                    Capability::Ref,
                    false,
                );
            }
            let (s, handler_ty) = self.infer_expr(&handler_ctx, &h.body)?;
            let handler_ty_subst = apply_subst(&handler_ty, &s);
            let body_ty_subst = apply_subst(&body_ty, &compose_subst(&s, &subst));
            let s_unify = mgu(&handler_ty_subst, &body_ty_subst, Span::default())?;
            subst = compose_subst(&s_unify, &compose_subst(&s, &subst));
        }

        Ok((subst.clone(), apply_subst(&body_ty, &subst)))
    }

    /// Infer while loop.
    fn infer_while(
        &mut self,
        ctx: &TypeContext,
        cond: &Expr,
        body: &Expr,
        span: Span,
    ) -> NuResult<(Substitution, Type)> {
        // Cond must be bool
        let (s1, cond_ty) = self.infer_expr(ctx, cond)?;
        let s2 = mgu(&cond_ty, &Type::Primitive(PrimitiveType::Bool), span)?;
        let s_combined = compose_subst(&s2, &s1);

        // Body can be anything
        let (s3, _body_ty) = self.infer_expr(ctx, body)?;
        let final_subst = compose_subst(&s3, &s_combined);

        Ok((final_subst, Type::unit()))
    }

    /// Infer for comprehension.
    fn infer_for(
        &mut self,
        ctx: &TypeContext,
        var: &str,
        iterable: &Expr,
        body: &Expr,
        span: Span,
    ) -> NuResult<(Substitution, Type)> {
        // Infer iterable type (should be array-like)
        let (s1, iter_ty) = self.infer_expr(ctx, iterable)?;

        let elem_var = Type::Var(TypeVar::fresh());
        let s2 = mgu(
            &apply_subst(&iter_ty, &s1),
            &Type::Array(Box::new(elem_var.clone())),
            span,
        )?;
        let s_combined = compose_subst(&s2, &s1);

        // Bind the loop variable
        let body_ctx = ctx.extend(
            var.to_string(),
            apply_subst(&elem_var, &s_combined),
            Capability::Ref,
            false,
        );

        // Infer body
        let (s3, _body_ty) = self.infer_expr(&body_ctx, body)?;
        let final_subst = compose_subst(&s3, &s_combined);

        // For returns Unit
        Ok((final_subst, Type::unit()))
    }

    // -----------------------------------------------------------------------
    // Generalization with tracked context vars
    // -----------------------------------------------------------------------

    /// Validate that a type is usable as an FFI parameter/return type in the MVP.
    /// Only primitive Int, Float, Bool, String, and Unit are supported.
    fn validate_ffi_type(&self, ty: &Type, span: Span) -> NuResult<()> {
        match ty {
            Type::Primitive(PrimitiveType::Int)
            | Type::Primitive(PrimitiveType::Float)
            | Type::Primitive(PrimitiveType::Bool)
            | Type::Primitive(PrimitiveType::String)
            | Type::Primitive(PrimitiveType::Unit) => Ok(()),
            _ => Err(NuError::TypeError {
                msg: format!(
                    "Unsupported FFI type: {}. Only Int, Float, Bool, String, and Unit are allowed in this MVP.",
                    ty
                ),
                span,
                expected_type: None,
                found_type: Some(format!("{}", ty)),
                similar_names: None,
            }),
        }
    }

    /// Generalize a type by abstracting over free variables not in the context.
    ///
    /// Value restriction: variables occurring under a `Reference` constructor
    /// are never quantified. The cell is created once at binding time and
    /// shared by every use of the binding, so generalizing it would let the
    /// same cell be used at incompatible types (e.g.
    /// `let r = &[] in { r = [1]; (*r)[0] == "s" }`).
    fn do_generalize(&self, ctx: &TypeContext, ty: &Type) -> Type {
        // Replace any Skolem constants with fresh type variables so they
        // become the function's polymorphic type parameters. Skolems are
        // rigid during body checking; after the body succeeds, they become
        // quantifiable.
        let mut skolem_to_var: FxHashMap<u64, TypeVar> = FxHashMap::default();
        let mut skolem_vars = Vec::new();
        collect_skolems(ty, &mut skolem_to_var, &mut skolem_vars);
        let body = if skolem_to_var.is_empty() {
            ty.clone()
        } else {
            replace_skolems_in_type(ty, &skolem_to_var)
        };

        let ty_fv: FxHashSet<TypeVar> = body.free_vars().into_iter().collect();
        let ctx_fv = self.get_ctx_free_vars(ctx);
        let ref_fv: FxHashSet<TypeVar> = body.ref_free_vars().into_iter().collect();
        let mut gen_vars: Vec<TypeVar> = ty_fv
            .difference(&ctx_fv)
            .copied()
            .filter(|v| !ref_fv.contains(v))
            .collect();
        // Skolem vars are always generalized (they represent type params).
        gen_vars.extend(skolem_vars);

        if gen_vars.is_empty() {
            body
        } else {
            Type::Scheme {
                vars: gen_vars,
                body: Box::new(body),
            }
        }
    }

    /// Collect self-referencing type variables from variant bodies.
    /// These appear as `App(Var(v), …)` constructors and must not be
    /// generalized — they're fixed points, not polymorphic variables.
    fn collect_recursive_vars(ty: &Type, out: &mut FxHashSet<TypeVar>) {
        match ty {
            Type::App { constructor, args } => {
                if let Type::Var(v) = constructor.as_ref() {
                    out.insert(*v);
                }
                Self::collect_recursive_vars(constructor, out);
                for a in args {
                    Self::collect_recursive_vars(a, out);
                }
            }
            Type::Variant(vs) => {
                for (_, p) in vs {
                    if let Some(p) = p {
                        Self::collect_recursive_vars(p, out);
                    }
                }
            }
            Type::Tuple(ts) => {
                for t in ts {
                    Self::collect_recursive_vars(t, out);
                }
            }
            Type::Record(fs) => {
                for (_, t) in fs {
                    Self::collect_recursive_vars(t, out);
                }
            }
            Type::Array(t) => Self::collect_recursive_vars(t, out),
            Type::Function { param, ret, .. } => {
                Self::collect_recursive_vars(param, out);
                Self::collect_recursive_vars(ret, out);
            }
            Type::Actor { state, behavior } => {
                Self::collect_recursive_vars(state, out);
                Self::collect_recursive_vars(behavior, out);
            }
            Type::Reference { inner, .. } => Self::collect_recursive_vars(inner, out),
            Type::Scheme { body, .. } => Self::collect_recursive_vars(body, out),
            Type::Nominal { underlying, .. } => Self::collect_recursive_vars(underlying, out),
            _ => {}
        }
    }

    /// Get free type variables from the context.
    fn get_ctx_free_vars(&self, ctx: &TypeContext) -> FxHashSet<TypeVar> {
        ctx.free_vars().into_iter().collect()
    }
}

/// Collect all unique Skolem IDs from a type and create a fresh TypeVar
/// mapping for each. Used by `do_generalize` to convert skolems back to
/// quantifiable type variables.
fn collect_skolems(ty: &Type, map: &mut FxHashMap<u64, TypeVar>, vars: &mut Vec<TypeVar>) {
    match ty {
        Type::Skolem(id) => {
            if !map.contains_key(id) {
                let tv = TypeVar::fresh();
                map.insert(*id, tv);
                vars.push(tv);
            }
        }
        Type::Tuple(ts) => {
            for t in ts {
                collect_skolems(t, map, vars);
            }
        }
        Type::Record(fs) => {
            for (_, t) in fs {
                collect_skolems(t, map, vars);
            }
        }
        Type::Variant(vs) => {
            for (_, t) in vs {
                if let Some(t) = t {
                    collect_skolems(t, map, vars);
                }
            }
        }
        Type::Array(t) => collect_skolems(t, map, vars),
        Type::Function { param, ret, .. } => {
            collect_skolems(param, map, vars);
            collect_skolems(ret, map, vars);
        }
        Type::Actor { state, behavior } => {
            collect_skolems(state, map, vars);
            collect_skolems(behavior, map, vars);
        }
        Type::App { constructor, args } => {
            collect_skolems(constructor, map, vars);
            for a in args {
                collect_skolems(a, map, vars);
            }
        }
        Type::Reference { inner, .. } => collect_skolems(inner, map, vars),
        Type::Scheme { body, .. } => collect_skolems(body, map, vars),
        Type::Nominal { underlying, .. } => collect_skolems(underlying, map, vars),
        _ => {}
    }
}

/// Replace every Skolem with its mapped TypeVar.
fn replace_skolems_in_type(ty: &Type, map: &FxHashMap<u64, TypeVar>) -> Type {
    match ty {
        Type::Var(v) => Type::Var(*v),
        Type::Primitive(_) => ty.clone(),
        Type::Skolem(id) => Type::Var(map.get(id).copied().unwrap_or_else(TypeVar::fresh)),
        Type::Tuple(ts) => {
            Type::Tuple(ts.iter().map(|t| replace_skolems_in_type(t, map)).collect())
        }
        Type::Record(fs) => Type::Record(
            fs.iter()
                .map(|(n, t)| (n.clone(), replace_skolems_in_type(t, map)))
                .collect(),
        ),
        Type::Variant(vs) => Type::Variant(
            vs.iter()
                .map(|(n, t)| {
                    (
                        n.clone(),
                        t.as_ref().map(|t| replace_skolems_in_type(t, map)),
                    )
                })
                .collect(),
        ),
        Type::Array(t) => Type::Array(Box::new(replace_skolems_in_type(t, map))),
        Type::Function {
            param,
            ret,
            effect,
            cap,
        } => Type::Function {
            param: Box::new(replace_skolems_in_type(param, map)),
            ret: Box::new(replace_skolems_in_type(ret, map)),
            effect: effect.clone(),
            cap: *cap,
        },
        Type::Actor { state, behavior } => Type::Actor {
            state: Box::new(replace_skolems_in_type(state, map)),
            behavior: Box::new(replace_skolems_in_type(behavior, map)),
        },
        Type::App { constructor, args } => Type::App {
            constructor: Box::new(replace_skolems_in_type(constructor, map)),
            args: args
                .iter()
                .map(|a| replace_skolems_in_type(a, map))
                .collect(),
        },
        Type::Reference { cap, inner } => Type::Reference {
            cap: *cap,
            inner: Box::new(replace_skolems_in_type(inner, map)),
        },
        Type::Scheme { vars, body } => Type::Scheme {
            vars: vars.clone(),
            body: Box::new(replace_skolems_in_type(body, map)),
        },
        Type::Nominal { name, underlying } => Type::Nominal {
            name: name.clone(),
            underlying: Box::new(replace_skolems_in_type(underlying, map)),
        },
    }
}
impl Default for TypeChecker {
    fn default() -> Self {
        Self::new()
    }
}

// ---------------------------------------------------------------------------
// Unit Tests
// ---------------------------------------------------------------------------

#[cfg(test)]
mod tests {
    use super::*;

    // Helper to create a span
    fn sp() -> Span {
        Span::new(0, 0)
    }

    // Helper to create an int literal expression
    fn int_lit(n: i64) -> Expr {
        Expr::Literal(Literal::Int(n), sp())
    }

    // Helper to create a bool literal
    fn bool_lit(b: bool) -> Expr {
        Expr::Literal(Literal::Bool(b), sp())
    }

    // Helper to create a string literal
    fn string_lit(s: &str) -> Expr {
        Expr::Literal(Literal::String(s.to_string()), sp())
    }

    // Helper to create a variable expression
    fn var(name: &str) -> Expr {
        Expr::Var(name.to_string(), sp())
    }

    // Helper to create a lambda
    fn lambda(param: &str, body: Expr) -> Expr {
        Expr::Lambda {
            ret_type: None,
            params: vec![Param::new(param, None)],
            body: Box::new(body),
            effect: None,
            span: sp(),
        }
    }

    // Helper to create application
    fn app(func: Expr, arg: Expr) -> Expr {
        Expr::App {
            func: Box::new(func),
            args: vec![arg],
            span: sp(),
        }
    }

    // Helper for let binding
    fn let_(name: &str, value: Expr, body: Expr) -> Expr {
        Expr::Let {
            name: name.to_string(),
            ty: None,
            value: Box::new(value),
            body: Box::new(body),
            mutable: false,
            span: sp(),
            let_in: false,
        }
    }

    // Helper for if
    fn if_(cond: Expr, then_: Expr, else_: Option<Expr>) -> Expr {
        Expr::If {
            cond: Box::new(cond),
            then_branch: Box::new(then_),
            else_branch: else_.map(Box::new),
            span: sp(),
        }
    }

    // Helper for binary op
    fn bin(op: BinOp, left: Expr, right: Expr) -> Expr {
        Expr::Binary {
            op,
            left: Box::new(left),
            right: Box::new(right),
            span: sp(),
        }
    }

    // Helper for tuple
    fn tuple(exprs: Vec<Expr>) -> Expr {
        Expr::Tuple(exprs, sp())
    }

    // Helper for record
    fn record(fields: Vec<(&str, Expr)>) -> Expr {
        Expr::Record(
            fields
                .into_iter()
                .map(|(n, e)| (n.to_string(), e))
                .collect(),
            sp(),
        )
    }

    // Helper for field access
    fn field(expr: Expr, name: &str) -> Expr {
        Expr::FieldAccess {
            expr: Box::new(expr),
            field: name.to_string(),
            span: sp(),
        }
    }

    // Helper to set up context with a typed binding
    fn ctx_with(name: &str, ty: Type) -> TypeContext {
        let mut ctx = TypeContext::new();
        ctx.bind(name.to_string(), ty, Capability::Ref, false);
        ctx
    }

    // -----------------------------------------------------------------------
    // Test: Literals
    // -----------------------------------------------------------------------

    #[test]
    fn test_infer_int_literal() {
        let mut tc = TypeChecker::new();
        let ctx = TypeContext::new();
        let (s, ty) = tc.infer_expr(&ctx, &int_lit(42)).unwrap();
        assert!(s.is_empty());
        assert_eq!(ty, Type::int());
    }

    #[test]
    fn test_infer_bool_literal() {
        let mut tc = TypeChecker::new();
        let ctx = TypeContext::new();
        let (s, ty) = tc.infer_expr(&ctx, &bool_lit(true)).unwrap();
        assert!(s.is_empty());
        assert_eq!(ty, Type::bool());
    }

    #[test]
    fn test_infer_string_literal() {
        let mut tc = TypeChecker::new();
        let ctx = TypeContext::new();
        let (s, ty) = tc.infer_expr(&ctx, &string_lit("hello")).unwrap();
        assert!(s.is_empty());
        assert_eq!(ty, Type::string());
    }

    #[test]
    fn test_infer_float_literal() {
        let mut tc = TypeChecker::new();
        let ctx = TypeContext::new();
        let expr = Expr::Literal(Literal::Float(2.5), sp());
        let (s, ty) = tc.infer_expr(&ctx, &expr).unwrap();
        assert!(s.is_empty());
        assert_eq!(ty, Type::float());
    }

    #[test]
    fn test_infer_unit_literal() {
        let mut tc = TypeChecker::new();
        let ctx = TypeContext::new();
        let expr = Expr::Literal(Literal::Unit, sp());
        let (s, ty) = tc.infer_expr(&ctx, &expr).unwrap();
        assert!(s.is_empty());
        assert_eq!(ty, Type::unit());
    }

    // -----------------------------------------------------------------------
    // Test: Variables
    // -----------------------------------------------------------------------

    #[test]
    fn test_infer_bound_variable() {
        let mut tc = TypeChecker::new();
        let ctx = ctx_with("x", Type::int());
        let (s, ty) = tc.infer_expr(&ctx, &var("x")).unwrap();
        assert!(s.is_empty());
        assert_eq!(ty, Type::int());
    }

    #[test]
    fn test_infer_unbound_variable() {
        let mut tc = TypeChecker::new();
        let ctx = TypeContext::new();
        let result = tc.infer_expr(&ctx, &var("undefined"));
        assert!(result.is_err());
    }

    #[test]
    fn test_infer_polymorphic_variable() {
        let mut tc = TypeChecker::new();
        // Bind 'id' as a polymorphic scheme: forall a. a -> a
        let a = TypeVar(100);
        let scheme = Type::Scheme {
            vars: vec![a],
            body: Box::new(Type::Function {
                param: Box::new(Type::Var(a)),
                ret: Box::new(Type::Var(a)),
                effect: EffectRow::empty(),
                cap: Capability::Ref,
            }),
        };
        let ctx = ctx_with("id", scheme);
        let (s, ty) = tc.infer_expr(&ctx, &var("id")).unwrap();
        assert!(s.is_empty());
        // Should be instantiated to a fresh function type
        match ty {
            Type::Function { param, ret, .. } => {
                // param and ret should be the same fresh variable
                assert_eq!(*param, *ret);
            }
            _ => panic!("Expected function type, got {:?}", ty),
        }
    }

    // -----------------------------------------------------------------------
    // Test: Lambda
    // -----------------------------------------------------------------------

    #[test]
    fn test_infer_identity_lambda() {
        let mut tc = TypeChecker::new();
        let ctx = TypeContext::new();
        let expr = lambda("x", var("x"));
        let (s, ty) = tc.infer_expr(&ctx, &expr).unwrap();
        assert!(s.is_empty());
        match ty {
            Type::Function { param, ret, .. } => {
                assert_eq!(*param, *ret);
            }
            _ => panic!("Expected function type, got {:?}", ty),
        }
    }

    #[test]
    fn test_infer_const_lambda() {
        let mut tc = TypeChecker::new();
        let ctx = TypeContext::new();
        let expr = lambda("x", int_lit(42));
        let (s, ty) = tc.infer_expr(&ctx, &expr).unwrap();
        assert!(s.is_empty());
        match ty {
            Type::Function { param: _, ret, .. } => {
                assert_eq!(*ret, Type::int());
            }
            _ => panic!("Expected function type, got {:?}", ty),
        }
    }

    // -----------------------------------------------------------------------
    // Test: Application
    // -----------------------------------------------------------------------

    #[test]
    fn test_infer_app_identity() {
        let mut tc = TypeChecker::new();
        let ctx = TypeContext::new();
        // (fn x => x)(42)
        let expr = app(lambda("x", var("x")), int_lit(42));
        let (s, ty) = tc.infer_expr(&ctx, &expr).unwrap();
        // Should infer Int (applying identity to 42)
        assert_eq!(apply_subst(&ty, &s), Type::int());
    }

    #[test]
    fn test_infer_app_const() {
        let mut tc = TypeChecker::new();
        let ctx = TypeContext::new();
        // (fn x => 42)("hello")
        let expr = app(lambda("x", int_lit(42)), string_lit("hello"));
        let (s, ty) = tc.infer_expr(&ctx, &expr).unwrap();
        // Should infer Int (const function ignores its argument)
        assert_eq!(apply_subst(&ty, &s), Type::int());
    }

    // -----------------------------------------------------------------------
    // Test: Let bindings
    // -----------------------------------------------------------------------

    #[test]
    fn test_infer_simple_let() {
        let mut tc = TypeChecker::new();
        let ctx = TypeContext::new();
        // let x = 42 in x
        let expr = let_("x", int_lit(42), var("x"));
        let (s, ty) = tc.infer_expr(&ctx, &expr).unwrap();
        assert_eq!(apply_subst(&ty, &s), Type::int());
    }

    #[test]
    fn test_infer_let_with_usage() {
        let mut tc = TypeChecker::new();
        let ctx = TypeContext::new();
        // let x = 42 in x + 1
        let expr = let_("x", int_lit(42), bin(BinOp::Add, var("x"), int_lit(1)));
        let (s, ty) = tc.infer_expr(&ctx, &expr).unwrap();
        assert_eq!(apply_subst(&ty, &s), Type::int());
    }

    #[test]
    fn test_infer_let_polymorphism() {
        let mut tc = TypeChecker::new();
        let ctx = TypeContext::new();
        // let id = fn x => x in (id(42), id(true))
        // This tests that 'id' is polymorphic
        let id = lambda("x", var("x"));
        let body = tuple(vec![
            app(var("id"), int_lit(42)),
            app(var("id"), bool_lit(true)),
        ]);
        let expr = let_("id", id, body);
        let (s, ty) = tc.infer_expr(&ctx, &expr).unwrap();
        match apply_subst(&ty, &s) {
            Type::Tuple(ts) if ts.len() == 2 => {
                assert_eq!(ts[0], Type::int());
                assert_eq!(ts[1], Type::bool());
            }
            other => panic!("Expected Tuple[Int, Bool], got {:?}", other),
        }
    }

    // -----------------------------------------------------------------------
    // Test: If expressions
    // -----------------------------------------------------------------------

    #[test]
    fn test_infer_if_then_else() {
        let mut tc = TypeChecker::new();
        let ctx = TypeContext::new();
        // if true then 42 else 0
        let expr = if_(bool_lit(true), int_lit(42), Some(int_lit(0)));
        let (s, ty) = tc.infer_expr(&ctx, &expr).unwrap();
        assert_eq!(apply_subst(&ty, &s), Type::int());
    }

    #[test]
    fn test_infer_if_with_condition_error() {
        let mut tc = TypeChecker::new();
        let ctx = TypeContext::new();
        // if 42 then 0 else 1 (condition must be bool)
        let expr = if_(int_lit(42), int_lit(0), Some(int_lit(1)));
        let result = tc.infer_expr(&ctx, &expr);
        assert!(result.is_err());
    }

    #[test]
    fn test_infer_if_branch_mismatch() {
        let mut tc = TypeChecker::new();
        let ctx = TypeContext::new();
        // if true then 42 else "hello" (branch mismatch)
        let expr = if_(bool_lit(true), int_lit(42), Some(string_lit("hello")));
        let result = tc.infer_expr(&ctx, &expr);
        assert!(result.is_err());
    }

    // -----------------------------------------------------------------------
    // Test: Binary operators
    // -----------------------------------------------------------------------

    #[test]
    fn test_infer_binop_arithmetic() {
        let mut tc = TypeChecker::new();
        let ctx = TypeContext::new();
        // 1 + 2
        let expr = bin(BinOp::Add, int_lit(1), int_lit(2));
        let (s, ty) = tc.infer_expr(&ctx, &expr).unwrap();
        assert_eq!(apply_subst(&ty, &s), Type::int());
    }

    #[test]
    fn test_infer_binop_comparison() {
        let mut tc = TypeChecker::new();
        let ctx = TypeContext::new();
        // 1 < 2
        let expr = bin(BinOp::Lt, int_lit(1), int_lit(2));
        let (s, ty) = tc.infer_expr(&ctx, &expr).unwrap();
        assert_eq!(apply_subst(&ty, &s), Type::bool());
    }

    #[test]
    fn test_infer_binop_boolean() {
        let mut tc = TypeChecker::new();
        let ctx = TypeContext::new();
        // true && false
        let expr = bin(BinOp::And, bool_lit(true), bool_lit(false));
        let (s, ty) = tc.infer_expr(&ctx, &expr).unwrap();
        assert_eq!(apply_subst(&ty, &s), Type::bool());
    }

    #[test]
    fn test_infer_binop_bitwise() {
        let mut tc = TypeChecker::new();
        let ctx = TypeContext::new();
        // 1 & 2
        let expr = bin(BinOp::BitAnd, int_lit(1), int_lit(2));
        let (s, ty) = tc.infer_expr(&ctx, &expr).unwrap();
        assert_eq!(apply_subst(&ty, &s), Type::int());
    }

    #[test]
    fn test_infer_binop_boolean_error() {
        let mut tc = TypeChecker::new();
        let ctx = TypeContext::new();
        // 1 && 2 (must be bool)
        let expr = bin(BinOp::And, int_lit(1), int_lit(2));
        let result = tc.infer_expr(&ctx, &expr);
        assert!(result.is_err());
    }

    // -----------------------------------------------------------------------
    // Test: Unary operators
    // -----------------------------------------------------------------------

    #[test]
    fn test_infer_unary_neg() {
        let mut tc = TypeChecker::new();
        let ctx = TypeContext::new();
        // -42
        let expr = Expr::Unary {
            op: UnOp::Neg,
            expr: Box::new(int_lit(42)),
            span: sp(),
        };
        let (s, ty) = tc.infer_expr(&ctx, &expr).unwrap();
        // Negation on Int should give Int
        assert_eq!(apply_subst(&ty, &s), Type::int());
    }

    #[test]
    fn test_infer_unary_not() {
        let mut tc = TypeChecker::new();
        let ctx = TypeContext::new();
        // !true
        let expr = Expr::Unary {
            op: UnOp::Not,
            expr: Box::new(bool_lit(true)),
            span: sp(),
        };
        let (s, ty) = tc.infer_expr(&ctx, &expr).unwrap();
        assert_eq!(apply_subst(&ty, &s), Type::bool());
    }

    // -----------------------------------------------------------------------
    // Test: Tuples
    // -----------------------------------------------------------------------

    #[test]
    fn test_infer_tuple() {
        let mut tc = TypeChecker::new();
        let ctx = TypeContext::new();
        // (42, true, "hello")
        let expr = tuple(vec![int_lit(42), bool_lit(true), string_lit("hello")]);
        let (s, ty) = tc.infer_expr(&ctx, &expr).unwrap();
        match apply_subst(&ty, &s) {
            Type::Tuple(ts) => {
                assert_eq!(ts.len(), 3);
                assert_eq!(ts[0], Type::int());
                assert_eq!(ts[1], Type::bool());
                assert_eq!(ts[2], Type::string());
            }
            other => panic!("Expected tuple, got {:?}", other),
        }
    }

    #[test]
    fn test_infer_empty_tuple() {
        let mut tc = TypeChecker::new();
        let ctx = TypeContext::new();
        let expr = tuple(vec![]);
        let (s, ty) = tc.infer_expr(&ctx, &expr).unwrap();
        match apply_subst(&ty, &s) {
            Type::Tuple(ts) => assert!(ts.is_empty()),
            other => panic!("Expected empty tuple, got {:?}", other),
        }
    }

    // -----------------------------------------------------------------------
    // Test: Records
    // -----------------------------------------------------------------------

    #[test]
    fn test_infer_record() {
        let mut tc = TypeChecker::new();
        let ctx = TypeContext::new();
        // { x: 42, y: true }
        let expr = record(vec![("x", int_lit(42)), ("y", bool_lit(true))]);
        let (s, ty) = tc.infer_expr(&ctx, &expr).unwrap();
        match apply_subst(&ty, &s) {
            Type::Record(fields) => {
                assert_eq!(fields.len(), 2);
                // Fields may be in any order
                let field_map: FxHashMap<String, Type> = fields.into_iter().collect();
                assert_eq!(field_map.get("x"), Some(&Type::int()));
                assert_eq!(field_map.get("y"), Some(&Type::bool()));
            }
            other => panic!("Expected record, got {:?}", other),
        }
    }

    // -----------------------------------------------------------------------
    // Test: Field access
    // -----------------------------------------------------------------------

    #[test]
    fn test_infer_field_access() {
        let mut tc = TypeChecker::new();
        let ctx = TypeContext::new();
        // { x: 42, y: true }.x
        let rec = record(vec![("x", int_lit(42)), ("y", bool_lit(true))]);
        let expr = field(rec, "x");
        let (s, ty) = tc.infer_expr(&ctx, &expr).unwrap();
        assert_eq!(apply_subst(&ty, &s), Type::int());
    }

    // -----------------------------------------------------------------------
    // Test: Recursive functions (let rec)
    // -----------------------------------------------------------------------

    #[test]
    fn test_infer_letrec() {
        let mut tc = TypeChecker::new();
        let ctx = TypeContext::new();
        // let rec fact n = if n == 0 then 1 else n * fact(n - 1) in fact(5)
        // (simplified: let rec f x = x in f(42))
        let body = var("x");
        let rec_expr = Expr::LetRec {
            name: "f".to_string(),
            params: vec![Param::new("x", None)],
            value: Box::new(body),
            body: Box::new(app(var("f"), int_lit(42))),
            span: sp(),
        };
        let (s, ty) = tc.infer_expr(&ctx, &rec_expr).unwrap();
        assert_eq!(apply_subst(&ty, &s), Type::int());
    }

    // -----------------------------------------------------------------------
    // Test: Block
    // -----------------------------------------------------------------------

    #[test]
    fn test_infer_block() {
        let mut tc = TypeChecker::new();
        let ctx = TypeContext::new();
        // { 1; 2; 3 }
        let expr = Expr::Block {
            exprs: vec![int_lit(1), int_lit(2), int_lit(3)],
            span: sp(),
        };
        let (s, ty) = tc.infer_expr(&ctx, &expr).unwrap();
        assert_eq!(apply_subst(&ty, &s), Type::int());
    }

    // -----------------------------------------------------------------------
    // Test: Array
    // -----------------------------------------------------------------------

    #[test]
    fn test_infer_array() {
        let mut tc = TypeChecker::new();
        let ctx = TypeContext::new();
        // [1, 2, 3]
        let expr = Expr::Array(vec![int_lit(1), int_lit(2), int_lit(3)], sp());
        let (s, ty) = tc.infer_expr(&ctx, &expr).unwrap();
        match apply_subst(&ty, &s) {
            Type::Array(elem_ty) => {
                assert_eq!(*elem_ty, Type::int());
            }
            other => panic!("Expected array, got {:?}", other),
        }
    }

    // -----------------------------------------------------------------------
    // Test: Pattern matching
    // -----------------------------------------------------------------------

    #[test]
    fn test_infer_match_wildcard() {
        let mut tc = TypeChecker::new();
        let ctx = TypeContext::new();
        // match 42 { | _ => 0 }
        let expr = Expr::Match {
            scrutinee: Box::new(int_lit(42)),
            arms: vec![(Pattern::Wild, None, int_lit(0))],
            span: sp(),
        };
        let (s, ty) = tc.infer_expr(&ctx, &expr).unwrap();
        assert_eq!(apply_subst(&ty, &s), Type::int());
    }

    #[test]
    fn test_infer_match_variable() {
        let mut tc = TypeChecker::new();
        let ctx = TypeContext::new();
        // match 42 { | x => x }
        let expr = Expr::Match {
            scrutinee: Box::new(int_lit(42)),
            arms: vec![(Pattern::Var("x".to_string()), None, var("x"))],
            span: sp(),
        };
        let (s, ty) = tc.infer_expr(&ctx, &expr).unwrap();
        assert_eq!(apply_subst(&ty, &s), Type::int());
    }

    // -----------------------------------------------------------------------
    // Test: Pipe operator
    // -----------------------------------------------------------------------

    #[test]
    fn test_infer_pipe() {
        let mut tc = TypeChecker::new();
        let ctx = TypeContext::new();
        // 42 |> (fn x => x)
        let expr = Expr::Pipe {
            left: Box::new(int_lit(42)),
            right: Box::new(lambda("x", var("x"))),
            span: sp(),
        };
        let (s, ty) = tc.infer_expr(&ctx, &expr).unwrap();
        assert_eq!(apply_subst(&ty, &s), Type::int());
    }

    // -----------------------------------------------------------------------
    // Test: Type annotation
    // -----------------------------------------------------------------------

    #[test]
    fn test_infer_type_annotate() {
        let mut tc = TypeChecker::new();
        let ctx = TypeContext::new();
        // 42 : Int
        let expr = Expr::TypeAnnotate {
            expr: Box::new(int_lit(42)),
            ty: Type::int(),
            span: sp(),
        };
        let (s, ty) = tc.infer_expr(&ctx, &expr).unwrap();
        assert_eq!(apply_subst(&ty, &s), Type::int());
    }

    #[test]
    fn test_infer_type_annotate_error() {
        let mut tc = TypeChecker::new();
        let ctx = TypeContext::new();
        // 42 : Bool (wrong annotation)
        let expr = Expr::TypeAnnotate {
            expr: Box::new(int_lit(42)),
            ty: Type::bool(),
            span: sp(),
        };
        let result = tc.infer_expr(&ctx, &expr);
        assert!(result.is_err());
    }

    // -----------------------------------------------------------------------
    // Test: Polymorphism
    // -----------------------------------------------------------------------

    #[test]
    fn test_polymorphism_twice() {
        let mut tc = TypeChecker::new();
        let ctx = TypeContext::new();
        // let f = fn x => x in let a = f(1) in let b = f(true) in (a, b)
        let f = lambda("x", var("x"));
        let inner = let_(
            "b",
            app(var("f"), bool_lit(true)),
            tuple(vec![var("a"), var("b")]),
        );
        let middle = let_("a", app(var("f"), int_lit(1)), inner);
        let expr = let_("f", f, middle);
        let (s, ty) = tc.infer_expr(&ctx, &expr).unwrap();
        match apply_subst(&ty, &s) {
            Type::Tuple(ts) => {
                assert_eq!(ts[0], Type::int());
                assert_eq!(ts[1], Type::bool());
            }
            other => panic!("Expected Tuple[Int, Bool], got {:?}", other),
        }
    }

    // -----------------------------------------------------------------------
    // Test: Substitution operations
    // -----------------------------------------------------------------------

    #[test]
    fn test_compose_subst() {
        let v1 = TypeVar(1);
        let v2 = TypeVar(2);
        let s1 = vec![(v1, Type::int())];
        let s2 = vec![(v2, Type::Var(v1))];
        let composed = compose_subst(&s2, &s1);
        // Applying composed to v2 should give Int (v1 -> Int, then v2 -> v1)
        let ty = apply_subst(&Type::Var(v2), &composed);
        assert_eq!(ty, Type::int());
    }

    #[test]
    fn test_mgu_same_type() {
        let t1 = Type::int();
        let t2 = Type::int();
        let s = mgu(&t1, &t2, sp()).unwrap();
        assert!(s.is_empty());
    }

    #[test]
    fn test_mgu_var_type() {
        let v = TypeVar(1);
        let t = Type::int();
        let s = mgu(&Type::Var(v), &t, sp()).unwrap();
        assert_eq!(s.len(), 1);
        assert_eq!(s[0].0, v);
        assert_eq!(s[0].1, Type::int());
    }

    #[test]
    fn test_mgu_function() {
        let v1 = TypeVar(1);
        let v2 = TypeVar(2);
        let f1 = Type::Function {
            param: Box::new(Type::Var(v1)),
            ret: Box::new(Type::Var(v1)),
            effect: EffectRow::empty(),
            cap: Capability::Ref,
        };
        let f2 = Type::Function {
            param: Box::new(Type::int()),
            ret: Box::new(Type::Var(v2)),
            effect: EffectRow::empty(),
            cap: Capability::Ref,
        };
        let s = mgu(&f1, &f2, sp()).unwrap();
        let result = apply_subst(&Type::Var(v2), &s);
        assert_eq!(result, Type::int());
    }

    #[test]
    fn test_mgu_opaque_nominal_rejects_underlying() {
        let html = Type::Nominal {
            name: "Html".to_string(),
            underlying: Box::new(Type::string()),
        };
        let s = Type::string();
        assert!(
            mgu(&html, &s, sp()).is_err(),
            "Html should not unify with String"
        );
    }

    #[test]
    fn test_mgu_opaque_nominal_accepts_same_name() {
        let html1 = Type::Nominal {
            name: "Html".to_string(),
            underlying: Box::new(Type::string()),
        };
        let html2 = Type::Nominal {
            name: "Html".to_string(),
            underlying: Box::new(Type::string()),
        };
        let s = mgu(&html1, &html2, sp()).unwrap();
        assert!(s.is_empty());
    }

    #[test]
    fn test_occurs_check() {
        let v = TypeVar(1);
        let t = Type::Function {
            param: Box::new(Type::Var(v)),
            ret: Box::new(Type::Var(v)),
            effect: EffectRow::empty(),
            cap: Capability::Ref,
        };
        let result = mgu(&Type::Var(v), &t, sp());
        assert!(result.is_err());
    }

    // -----------------------------------------------------------------------
    // Test: Module checking
    // -----------------------------------------------------------------------

    #[test]
    fn test_check_empty_module() {
        let mut tc = TypeChecker::new();
        let module = AstModule {
            name: "test".to_string(),
            decls: vec![],
        };
        let ty = tc.check_module(&module).unwrap();
        assert_eq!(ty, Type::unit());
    }

    #[test]
    fn test_check_module_with_function() {
        let mut tc = TypeChecker::new();
        let module = AstModule {
            name: "test".to_string(),
            decls: vec![Decl::Function {
                name: "add1".to_string(),
                type_params: vec![],
                type_param_constraints: vec![],
                params: vec![Param::new("x", Some(Type::int()))],
                default_values: vec![None],
                using_params: vec![],
                ret_type: Some(Type::int()),
                error_type: None,
                effect: None,
                cap: None,
                requires: vec![],
                ensures: vec![],
                body: bin(BinOp::Add, var("x"), int_lit(1)),
                annotations: vec![],
                public: true,
                span: sp(),
            }],
        };
        let ty = tc.check_module(&module).unwrap();
        match ty {
            Type::Function { param, ret, .. } => {
                assert_eq!(*param, Type::int());
                assert_eq!(*ret, Type::int());
            }
            other => panic!("Expected function type, got {:?}", other),
        }
    }

    #[test]
    fn test_nested_module_exports_to_enclosing_scope() {
        // module Foo { fn bar() { 42 } }
        // bar()
        let mut tc = TypeChecker::new();
        let module = AstModule {
            name: "test".to_string(),
            decls: vec![
                Decl::Module {
                    name: "Foo".to_string(),
                    exports: vec![],
                    decls: vec![Decl::Function {
                        name: "bar".to_string(),
                        type_params: vec![],
                        type_param_constraints: vec![],
                        params: vec![],
                        default_values: vec![],
                        using_params: vec![],
                        ret_type: Some(Type::int()),
                        error_type: None,
                        effect: None,
                        cap: None,
                        requires: vec![],
                        ensures: vec![],
                        body: int_lit(42),
                        annotations: vec![],
                        public: true,
                        span: sp(),
                    }],
                    span: sp(),
                },
                Decl::Function {
                    name: "main".to_string(),
                    type_params: vec![],
                    type_param_constraints: vec![],
                    params: vec![],
                    default_values: vec![],
                    using_params: vec![],
                    ret_type: None,
                    error_type: None,
                    effect: None,
                    cap: None,
                    requires: vec![],
                    ensures: vec![],
                    body: Expr::App {
                        func: Box::new(var("bar")),
                        args: vec![],
                        span: sp(),
                    },
                    annotations: vec![],
                    public: true,
                    span: sp(),
                },
            ],
        };
        let ty = tc.check_module(&module).unwrap();
        assert_eq!(
            ty,
            Type::Function {
                param: Box::new(Type::Tuple(vec![])),
                ret: Box::new(Type::int()),
                effect: EffectRow::empty(),
                cap: Capability::Ref,
            }
        );
    }

    #[test]
    fn test_nested_module_siblings_see_each_other() {
        // module Foo { fn bar() { 42 } fn baz() { bar() } }
        let mut tc = TypeChecker::new();
        let module = AstModule {
            name: "test".to_string(),
            decls: vec![Decl::Module {
                name: "Foo".to_string(),
                exports: vec![],
                decls: vec![
                    Decl::Function {
                        name: "bar".to_string(),
                        type_params: vec![],
                        type_param_constraints: vec![],
                        params: vec![],
                        default_values: vec![],
                        using_params: vec![],
                        ret_type: Some(Type::int()),
                        error_type: None,
                        effect: None,
                        cap: None,
                        requires: vec![],
                        ensures: vec![],
                        body: int_lit(42),
                        annotations: vec![],
                        public: true,
                        span: sp(),
                    },
                    Decl::Function {
                        name: "baz".to_string(),
                        type_params: vec![],
                        type_param_constraints: vec![],
                        params: vec![],
                        default_values: vec![],
                        using_params: vec![],
                        ret_type: None,
                        error_type: None,
                        effect: None,
                        cap: None,
                        requires: vec![],
                        ensures: vec![],
                        body: Expr::App {
                            func: Box::new(var("bar")),
                            args: vec![],
                            span: sp(),
                        },
                        annotations: vec![],
                        public: true,
                        span: sp(),
                    },
                ],
                span: sp(),
            }],
        };
        let ty = tc.check_module(&module).unwrap();
        match ty {
            Type::Function { ret, .. } => assert_eq!(*ret, Type::int()),
            other => panic!("Expected function type, got {:?}", other),
        }
    }

    // -----------------------------------------------------------------------
    // Test: Generalization and Instantiation
    // -----------------------------------------------------------------------

    #[test]
    fn test_instantiate_scheme() {
        let a = TypeVar(100);
        let scheme = Type::Scheme {
            vars: vec![a],
            body: Box::new(Type::Function {
                param: Box::new(Type::Var(a)),
                ret: Box::new(Type::Var(a)),
                effect: EffectRow::empty(),
                cap: Capability::Ref,
            }),
        };
        let instantiated = instantiate(&scheme);
        match instantiated {
            Type::Function { param, ret, .. } => {
                // After instantiation, param and ret should be equal fresh vars
                assert_eq!(*param, *ret);
                // And different from the original
                assert_ne!(*param, Type::Var(a));
            }
            _ => panic!("Expected function type"),
        }
    }

    // -----------------------------------------------------------------------
    // Test: Reference types
    // -----------------------------------------------------------------------

    #[test]
    fn test_infer_ref() {
        let mut tc = TypeChecker::new();
        let ctx = TypeContext::new();
        // ref(42)
        let expr = Expr::Unary {
            op: UnOp::Ref(Capability::Ref),
            expr: Box::new(int_lit(42)),
            span: sp(),
        };
        let (s, ty) = tc.infer_expr(&ctx, &expr).unwrap();
        match apply_subst(&ty, &s) {
            Type::Reference { cap, inner } => {
                assert_eq!(cap, Capability::Ref);
                assert_eq!(*inner, Type::int());
            }
            other => panic!("Expected reference type, got {:?}", other),
        }
    }

    #[test]
    fn test_infer_deref() {
        let mut tc = TypeChecker::new();
        let ctx = ctx_with(
            "x",
            Type::Reference {
                cap: Capability::Ref,
                inner: Box::new(Type::int()),
            },
        );
        let expr = Expr::Unary {
            op: UnOp::Deref,
            expr: Box::new(var("x")),
            span: sp(),
        };
        let (s, ty) = tc.infer_expr(&ctx, &expr).unwrap();
        assert_eq!(apply_subst(&ty, &s), Type::int());
    }

    // -----------------------------------------------------------------------
    // Test: Effect row handling in application and declarations
    // -----------------------------------------------------------------------

    #[test]
    fn test_infer_app_preserves_lambda_effect() {
        let mut tc = TypeChecker::new();
        let ctx = TypeContext::new();
        let lam = Expr::Lambda {
            ret_type: None,
            params: vec![Param::new("x", Some(Type::int()))],
            body: Box::new(var("x")),
            effect: Some(EffectRow::Closed(vec![Effect::IO])),
            span: sp(),
        };
        let app = Expr::App {
            func: Box::new(lam),
            args: vec![int_lit(1)],
            span: sp(),
        };
        let (s, ty) = tc.infer_expr(&ctx, &app).unwrap();
        assert_eq!(apply_subst(&ty, &s), Type::int());
    }

    #[test]
    fn test_infer_function_decl_with_effect() {
        let mut tc = TypeChecker::new();
        let module = AstModule {
            name: "test".to_string(),
            decls: vec![Decl::Function {
                name: "io_fn".to_string(),
                type_params: vec![],
                type_param_constraints: vec![],
                params: vec![Param::new("x", Some(Type::int()))],
                default_values: vec![None],
                using_params: vec![],
                ret_type: Some(Type::int()),
                error_type: None,
                effect: Some(EffectRow::Closed(vec![Effect::IO])),
                cap: None,
                requires: vec![],
                ensures: vec![],
                body: bin(BinOp::Add, var("x"), int_lit(1)),
                annotations: vec![],
                public: true,
                span: sp(),
            }],
        };
        let ty = tc.check_module(&module).unwrap();
        match ty {
            Type::Function { effect, .. } => {
                assert!(effect.contains(&Effect::IO));
            }
            other => panic!("Expected function type, got {:?}", other),
        }
    }

    #[test]
    fn test_infer_handle_checks_handler_body() {
        let mut tc = TypeChecker::new();
        let ctx = TypeContext::new();
        let handle_ok = Expr::Handle {
            body: Box::new(int_lit(42)),
            handlers: vec![EffectHandler {
                effect_name: "IO".to_string(),
                op_name: "print".to_string(),
                params: vec!["msg".to_string()],
                body: int_lit(0),
                resume: false,
            }],
            span: sp(),
        };
        let (s, ty) = tc.infer_expr(&ctx, &handle_ok).unwrap();
        assert_eq!(apply_subst(&ty, &s), Type::int());

        let handle_bad = Expr::Handle {
            body: Box::new(int_lit(42)),
            handlers: vec![EffectHandler {
                effect_name: "IO".to_string(),
                op_name: "print".to_string(),
                params: vec!["msg".to_string()],
                body: string_lit("oops"),
                resume: false,
            }],
            span: sp(),
        };
        assert!(tc.infer_expr(&ctx, &handle_bad).is_err());
    }

    #[test]
    fn test_extern_function_available_with_ffi_effect() {
        let module = AstModule {
            name: "main".to_string(),
            decls: vec![
                Decl::Extern {
                    library: "libm.so.6".to_string(),
                    funcs: vec![ExternFunc {
                        name: "sqrt".to_string(),
                        params: vec![("x".to_string(), Type::float())],
                        ret: Type::float(),
                        span: sp(),
                    }],
                    span: sp(),
                },
                Decl::Function {
                    name: "use_sqrt".to_string(),
                    type_params: vec![],
                    type_param_constraints: vec![],
                    params: vec![],
                    default_values: vec![],
                    using_params: vec![],
                    ret_type: None,
                    error_type: None,
                    effect: None,
                    cap: None,
                    requires: vec![],
                    ensures: vec![],
                    body: Expr::App {
                        func: Box::new(Expr::Var("sqrt".to_string(), sp())),
                        args: vec![Expr::Literal(Literal::Float(4.0), sp())],
                        span: sp(),
                    },
                    annotations: vec![],
                    public: false,
                    span: sp(),
                },
            ],
        };
        let mut tc = TypeChecker::new();
        let ty = tc.check_module(&module).unwrap();
        // use_sqrt is a parameterless function returning Float.
        match ty {
            Type::Function { param, ret, .. } => {
                assert_eq!(*param, Type::Tuple(vec![]));
                assert_eq!(*ret, Type::float());
            }
            other => panic!("Expected function type, got {:?}", other),
        }
    }

    #[test]
    fn test_extern_function_type_has_ffi_effect() {
        let mut tc = TypeChecker::new();
        let mut ctx = TypeContext::new();
        let extern_ty = Type::Function {
            param: Box::new(Type::float()),
            ret: Box::new(Type::float()),
            effect: EffectRow::singleton(Effect::FFI),
            cap: Capability::Ref,
        };
        ctx.bind("sqrt", extern_ty, Capability::Ref, false);
        let (_s, ty) = tc.infer_expr(&ctx, &var("sqrt")).unwrap();
        match ty {
            Type::Function { effect, .. } => {
                assert!(effect.contains(&Effect::FFI));
            }
            other => panic!("Expected function type, got {:?}", other),
        }
    }

    #[test]
    fn test_extern_unsupported_param_type_errors() {
        let module = AstModule {
            name: "main".to_string(),
            decls: vec![Decl::Extern {
                library: "lib".to_string(),
                funcs: vec![ExternFunc {
                    name: "bad".to_string(),
                    params: vec![("x".to_string(), Type::Array(Box::new(Type::int())))],
                    ret: Type::int(),
                    span: sp(),
                }],
                span: sp(),
            }],
        };
        let mut tc = TypeChecker::new();
        let result = tc.check_module(&module);
        assert!(result.is_err());
        match result.unwrap_err() {
            NuError::TypeError { msg, .. } => {
                assert!(msg.contains("Unsupported FFI type"));
                assert!(
                    msg.contains("[Int]"),
                    "Expected [Int] in FFI error, got: {}",
                    msg
                );
            }
            other => panic!("Expected TypeError, got {:?}", other),
        }
    }

    #[test]
    fn test_extern_unsupported_return_type_errors() {
        let module = AstModule {
            name: "main".to_string(),
            decls: vec![Decl::Extern {
                library: "lib".to_string(),
                funcs: vec![ExternFunc {
                    name: "bad".to_string(),
                    params: vec![("x".to_string(), Type::int())],
                    ret: Type::Record(vec![("a".to_string(), Type::int())]),
                    span: sp(),
                }],
                span: sp(),
            }],
        };
        let mut tc = TypeChecker::new();
        let result = tc.check_module(&module);
        assert!(result.is_err());
        match result.unwrap_err() {
            NuError::TypeError { msg, .. } => {
                assert!(msg.contains("Unsupported FFI type"));
            }
            other => panic!("Expected TypeError, got {:?}", other),
        }
    }

    // -----------------------------------------------------------------------
    // Test: substitution soundness regressions
    // -----------------------------------------------------------------------

    // Helper to lex, parse, and type-check a source string, mirroring the
    // `--check` pipeline in main.rs (frontend only, no effects/capabilities).
    fn check_src(src: &str) -> NuResult<Type> {
        let mut lexer = crate::lexer::Lexer::new(src);
        let tokens = lexer.lex()?;
        let mut parser = crate::parser::Parser::new(tokens);
        let module = parser.parse_module()?;
        let mut tc = TypeChecker::new();
        tc.check_module(&module)
    }

    #[test]
    fn test_apply_subst_to_ctx_updates_bindings() {
        let v = TypeVar(9001);
        let ctx = ctx_with("x", Type::Var(v));
        let subst = vec![(v, Type::int())];
        let updated = apply_subst_to_ctx(&ctx, &subst);
        match updated.lookup("x") {
            Some((ty, _, _)) => assert_eq!(*ty, Type::int()),
            None => panic!("binding for x lost"),
        }
    }

    #[test]
    fn test_compose_subst_merges_conflicting_bindings() {
        // s1: a := (b, c); s2: a := (Int, Bool). Composition must propagate
        // b := Int and c := Bool instead of discarding s2's mapping for a.
        let a = TypeVar(9101);
        let b = TypeVar(9102);
        let c = TypeVar(9103);
        let s1 = vec![(a, Type::Tuple(vec![Type::Var(b), Type::Var(c)]))];
        let s2 = vec![(a, Type::Tuple(vec![Type::int(), Type::bool()]))];
        let composed = compose_subst(&s2, &s1);
        assert_eq!(apply_subst(&Type::Var(a), &composed), s2[0].1);
        assert_eq!(apply_subst(&Type::Var(b), &composed), Type::int());
        assert_eq!(apply_subst(&Type::Var(c), &composed), Type::bool());
    }

    #[test]
    fn test_if_condition_constraint_reaches_else_branch() {
        // Regression: `apply_subst_to_ctx` was a no-op, so the Bool constraint
        // on `x` from the condition never reached `x + 1` in the else branch,
        // and `compose_subst` then silently dropped the Int constraint.
        // `let f = fn(x) if x then 1 else x + 1 in f(false)` must NOT check.
        let result = check_src("let f = fn(x) if x then 1 else x + 1 in f(false)");
        assert!(
            result.is_err(),
            "expected a type error, got {:?}",
            result.ok()
        );
    }

    #[test]
    fn test_ref_binding_is_not_generalized() {
        // Regression (value restriction): `&[]` was generalized to
        // `forall a. &[a]`, letting the same cell be used as both [Int] and
        // [String]. This must now fail to check.
        let result = check_src("let r = &[] in { r = [1]; (*r)[0] == \"s\" }");
        assert!(
            result.is_err(),
            "expected a type error, got {:?}",
            result.ok()
        );
    }

    #[test]
    fn test_polymorphic_let_still_generalizes() {
        // A let-bound lambda with no ambient free variables is still
        // polymorphic: each use instantiates fresh variables.
        let result = check_src("let id = fn(x) x in (id(1), id(true))");
        assert!(result.is_ok(), "expected success, got {:?}", result.err());
    }

    #[test]
    fn test_ctx_free_vars_prevent_overgeneralization() {
        // A let binding whose type shares a variable with the enclosing
        // context must not quantify that variable: two uses of `y` below
        // force it to be both Int and String, which is unsound if the
        // identity `id` were generalized over `y`'s variable.
        let result = check_src("fn f(y) { let id = fn(x) x in { id(y) + 1; id(y) == \"s\" } }");
        assert!(
            result.is_err(),
            "expected a type error, got {:?}",
            result.ok()
        );
    }

    #[test]
    fn test_do_generalize_skips_ctx_free_vars() {
        let tc = TypeChecker::new();
        let v = TypeVar(9201);
        let w = TypeVar(9202);
        let ctx = ctx_with("y", Type::Var(v));
        let ty = Type::Function {
            param: Box::new(Type::Var(v)),
            ret: Box::new(Type::Var(w)),
            effect: EffectRow::empty(),
            cap: Capability::Ref,
        };
        match tc.do_generalize(&ctx, &ty) {
            // v is free in the context, so only w is quantified.
            Type::Scheme { vars, .. } => {
                assert!(!vars.contains(&v));
                assert!(vars.contains(&w));
            }
            other => panic!("expected a scheme quantifying w, got {:?}", other),
        }
    }

    #[test]
    fn test_do_generalize_skips_ref_vars() {
        let tc = TypeChecker::new();
        let v = TypeVar(9301);
        let ctx = TypeContext::new();
        // A bare reference type: v must not be quantified.
        let ref_ty = Type::Reference {
            cap: Capability::Ref,
            inner: Box::new(Type::Array(Box::new(Type::Var(v)))),
        };
        match tc.do_generalize(&ctx, &ref_ty) {
            Type::Scheme { .. } => panic!("ref-typed binding must not be generalized"),
            other => assert_eq!(other, ref_ty),
        }
        // A function returning a reference creates the cell per call, so v
        // is still safe to quantify.
        let mk_ty = Type::Function {
            param: Box::new(Type::unit()),
            ret: Box::new(Type::Reference {
                cap: Capability::Ref,
                inner: Box::new(Type::Var(v)),
            }),
            effect: EffectRow::empty(),
            cap: Capability::Ref,
        };
        match tc.do_generalize(&ctx, &mk_ty) {
            Type::Scheme { vars, .. } => assert!(vars.contains(&v)),
            other => panic!("expected a scheme quantifying v, got {:?}", other),
        }
    }

    // -----------------------------------------------------------------------
    // Test: declared types — variants, aliases, records, Nil (SPEC2 §3.4.1)
    // -----------------------------------------------------------------------

    #[test]
    fn test_variant_constructors_are_bound() {
        // Declaring `Option` binds `Some` (payload -> variant function) and
        // `None` (variant value), so constructing with them checks.
        let result = check_src("type Option[T] = Some(T) | None\nSome(1)");
        assert!(result.is_ok(), "expected success, got {:?}", result.err());
    }

    #[test]
    fn test_unbound_variant_constructor_errors() {
        // Without a declaring variant type, `Some` stays an unbound variable.
        let result = check_src("Some(1)");
        assert!(
            result.is_err(),
            "expected unbound variable error, got {:?}",
            result.ok()
        );
    }

    #[test]
    fn test_nullary_constructor_binds_as_value() {
        let result = check_src("type Color = Red | Green | Blue\nRed");
        assert!(result.is_ok(), "expected success, got {:?}", result.err());
    }

    #[test]
    fn test_variant_branches_unify() {
        // The canonical Option pattern: `Some(1)` and `None` must unify
        // (constructor instantiation + variant unification), and the result
        // must unify with the expanded `Option[Int]` return annotation.
        let result = check_src(
            "type Option[T] = Some(T) | None\nfn pick(b: Bool) -> Option[Int] { if b then Some(1) else None }\npick",
        );
        assert!(result.is_ok(), "expected success, got {:?}", result.err());
    }

    #[test]
    fn test_variant_match_binds_payload() {
        let result = check_src(
            "type Option[T] = Some(T) | None\nfn get(o: Option[Int]) -> Int { match o with { | Some(v) => v | None => 0 } }\nget",
        );
        assert!(result.is_ok(), "expected success, got {:?}", result.err());
    }

    #[test]
    fn test_type_alias_expands_in_annotations() {
        // `MyInt` expands to `Int`: an Int argument checks...
        let ok = check_src("type alias MyInt = Int\nfn f(x: MyInt) -> MyInt { x }\nf(1)");
        assert!(ok.is_ok(), "expected success, got {:?}", ok.err());
        // ...and a String argument does not — the alias really constrains.
        let bad = check_src("type alias MyInt = Int\nfn f(x: MyInt) -> MyInt { x }\nf(\"s\")");
        assert!(
            bad.is_err(),
            "alias must constrain to the aliased type, got {:?}",
            bad.ok()
        );
    }

    #[test]
    fn test_hide_and_seal_scope_directives() {
        // `hide` blocks the named identifiers from resolution.
        let ok = check_src("let secret = 1 in hide secret { 42 }");
        assert!(
            ok.is_ok(),
            "hide must allow a body that avoids the hidden name"
        );
        let bad = check_src("let secret = 1 in hide secret { secret }");
        assert!(
            bad.is_err(),
            "referencing a hidden name must be an unbound-variable error"
        );

        // `seal except` whitelists the named identifiers, hiding the rest.
        let ok = check_src("let a = 1 in let b = 2 in seal except a { a }");
        assert!(ok.is_ok(), "seal except must allow the listed name");
        let bad = check_src("let a = 1 in let b = 2 in seal except a { b }");
        assert!(bad.is_err(), "seal except must block non-listed names");
    }

    #[test]
    fn test_record_type_name_usable_in_annotation() {
        let result =
            check_src("type Point = { x: Int, y: Int }\nfn get_x(p: Point) -> Int { p.x }\nget_x");
        assert!(result.is_ok(), "expected success, got {:?}", result.err());
    }

    #[test]
    fn test_same_row_var_record_unify_populates_structured_fields() {
        // Two open records sharing the SAME row variable but demanding
        // disjoint extra fields cannot be reconciled. This branch
        // (`r1 == r2`) is unreachable from ordinary source (generalization
        // mints a fresh row var per open record), so drive `unify_open_records`
        // directly and assert the structured TypeError fields are populated.
        let row = TypeVar(7781);
        let fs1 = vec![("x".to_string(), Type::int())];
        let fs2 = vec![("y".to_string(), Type::int())];
        let err = unify_open_records(
            &fs1,
            &Some(Type::Var(row)),
            &fs2,
            &Some(Type::Var(row)),
            Span::new(1, 2),
        )
        .unwrap_err();
        match err {
            NuError::TypeError {
                expected_type,
                found_type,
                ..
            } => {
                assert_eq!(expected_type.as_deref(), Some("record with fields {x}"));
                assert_eq!(found_type.as_deref(), Some("record with fields {y}"));
            }
            other => panic!("expected structured TypeError, got {:?}", other),
        }
    }

    #[test]
    fn test_residual_row_tail_record_unify_populates_structured_fields() {
        // A residual (non-variable) row tail cannot absorb fields — the `_`
        // fallback arm. Also unreachable from ordinary source; drive directly.
        let fs1 = vec![("a".to_string(), Type::int())];
        let fs2 = vec![("b".to_string(), Type::int())];
        let err = unify_open_records(
            &fs1,
            &Some(Type::int()),
            &fs2,
            &Some(Type::int()),
            Span::new(3, 4),
        )
        .unwrap_err();
        match err {
            NuError::TypeError {
                expected_type,
                found_type,
                ..
            } => {
                assert_eq!(expected_type.as_deref(), Some("record with fields {a}"));
                assert_eq!(found_type.as_deref(), Some("record with fields {b}"));
            }
            other => panic!("expected structured TypeError, got {:?}", other),
        }
    }

    #[test]
    fn test_unknown_type_name_in_annotation_errors() {
        let result = check_src("fn f(x: Bogus) x\nf(1)");
        match result {
            Err(NuError::ParseError { msg, .. }) => {
                assert!(
                    msg.contains("Unknown type name") && msg.contains("Bogus"),
                    "unexpected message: {}",
                    msg
                );
            }
            other => panic!("expected unknown type name parse error, got {:?}", other),
        }
    }

    #[test]
    fn test_nil_annotation_rejects_int() {
        let result = check_src("fn f(x: Nil) x\nf(1)");
        assert!(
            result.is_err(),
            "Int must not unify with Nil, got {:?}",
            result.ok()
        );
    }

    #[test]
    fn test_nil_annotation_accepts_nil() {
        let result = check_src("fn f(x: Nil) x\nf(nil)");
        assert!(result.is_ok(), "expected success, got {:?}", result.err());
    }

    #[test]
    fn test_state_machine_typechecks_like_actor() {
        // The desugared form is checked exactly like an actor: a string-tagged
        // `_sm_state` field, one behavior per event, hook bodies included. A
        // hook body ending in `nil` must not trip the no-else `if` Unit rule
        // (the desugar discards hook values into a trailing `unit`).
        let ty = check_src(
            r#"
            state_machine TcpConnection {
                state Closed
                state Connected
                event connect(address): Connected
                event disconnect: Closed
                on_entry Connected { nil }
                on_exit Connected { nil }
            }
            "#,
        )
        .unwrap();
        assert!(matches!(ty, Type::Actor { .. }));
    }

    #[test]
    fn test_state_machine_binds_actor_type_for_spawn_and_send() {
        // The machine name binds an actor type, so `spawn`/`send`/`ask`
        // against it check exactly like against a hand-written actor.
        let result = check_src(
            r#"
            state_machine M {
                state A
                state B
                event go: B
            }
            let m = spawn M {} in {
                send m go()
                ask m go()
            }
            "#,
        );
        assert!(result.is_ok(), "expected success, got {:?}", result.err());
    }

    #[test]
    fn test_emit_unknown_event_in_entity_is_error() {
        // `emit UnknownEvent` inside an entity that only declares KnownEvent
        // must produce a TypeError.
        let result = check_src(
            r#"
            entity E {
                state x: Int = 0
                events
                    | KnownEvent(n: Int)
                behavior go() { emit UnknownEvent(1) }
            }
            "#,
        );
        assert!(result.is_err(), "unknown event must be a type error");
        let error = result.unwrap_err();
        let err = format!("{}", error);
        assert!(
            err.contains("UnknownEvent"),
            "error must name the bad event: {}",
            err
        );
        assert!(
            err.contains("KnownEvent"),
            "error must list available events: {}",
            err
        );
        match error {
            NuError::TypeError {
                expected_type,
                found_type,
                similar_names,
                ..
            } => {
                assert_eq!(expected_type.as_deref(), Some("declared event name"));
                assert_eq!(found_type.as_deref(), Some("UnknownEvent"));
                assert_eq!(
                    similar_names.as_deref(),
                    Some(["KnownEvent".to_string()].as_slice())
                );
            }
            other => panic!("expected structured TypeError, got {:?}", other),
        }
    }

    #[test]
    fn test_emit_known_event_in_entity_passes() {
        // `emit KnownEvent` inside an entity that declares it must pass.
        let result = check_src(
            r#"
            entity E {
                state x: Int = 0
                events
                    | KnownEvent(n: Int)
                behavior go() { emit KnownEvent(42) }
            }
            "#,
        );
        assert!(result.is_ok(), "known event must pass: {:?}", result.err());
    }

    #[test]
    fn test_emit_wrong_arg_count_is_error() {
        let result = check_src(
            r#"
            entity E {
                state x: Int = 0
                events
                    | Ev(a: Int, b: Int)
                behavior go() { emit Ev(1) }
            }
            "#,
        );
        assert!(result.is_err(), "wrong arg count must be a type error");
        let err = format!("{}", result.unwrap_err());
        assert!(
            err.contains("expects 2 argument"),
            "error must mention arg count: {}",
            err
        );
    }

    // -----------------------------------------------------------------------
    // Range expression typechecking
    #[test]
    fn test_range_has_array_type() {
        // 0 .. 5 : Array[Int]
        let mut tc = TypeChecker::new();
        let expr = bin(BinOp::Range, int_lit(0), int_lit(5));
        let (s, ty) = tc.infer_expr(&TypeContext::new(), &expr).unwrap();
        assert_eq!(apply_subst(&ty, &s), Type::Array(Box::new(Type::int())));
    }

    #[test]
    fn test_range_rejects_non_int_left() {
        // true .. 5 should fail
        let mut tc = TypeChecker::new();
        let expr = bin(BinOp::Range, bool_lit(true), int_lit(5));
        let result = tc.infer_expr(&TypeContext::new(), &expr);
        assert!(result.is_err());
    }

    #[test]
    fn test_range_rejects_non_int_right() {
        // 0 .. "hello" should fail
        let mut tc = TypeChecker::new();
        let expr = bin(BinOp::Range, int_lit(0), string_lit("hello"));
        let result = tc.infer_expr(&TypeContext::new(), &expr);
        assert!(result.is_err());
    }
}
