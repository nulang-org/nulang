//! Constraint solver for Nulang's existing open effect-row representation.
//!
//! This module deliberately does **not** redefine source-level effect
//! assignability. Solving an existential row equality is different from
//! proving that an unconstrained callback satisfies a closed effect budget.
//! Before using solved bindings in the compiler, the typechecker must carry
//! the solver state and substitute it through all dependent function types.

use crate::types::{Effect, EffectRow, Region, Type};
use rustc_hash::{FxHashMap, FxHashSet};

/// A constraint that cannot be satisfied without changing the known effects.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum EffectRowSolveError {
    IncompatibleClosedRows {
        left: Vec<Effect>,
        right: Vec<Effect>,
    },
    CyclicTail(Region),
}

impl std::fmt::Display for EffectRowSolveError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::IncompatibleClosedRows { left, right } => {
                write!(f, "incompatible effect rows: {left:?} vs {right:?}")
            }
            Self::CyclicTail(region) => {
                write!(
                    f,
                    "effect-row tail {region:?} occurs in its own substitution"
                )
            }
        }
    }
}

impl std::error::Error for EffectRowSolveError {}

/// Compiler-owned, per-inference-context substitutions for effect-row tails.
///
/// Each successful mutation is atomic; failed occurs checks and incompatible
/// equations leave all earlier proven substitutions unchanged.
#[derive(Debug, Clone, Default)]
pub struct EffectRowSolver {
    bindings: FxHashMap<Region, EffectRow>,
}

fn sorted(mut effects: Vec<Effect>) -> Vec<Effect> {
    effects.sort_by_cached_key(ToString::to_string);
    effects.dedup();
    effects
}

fn union(left: &[Effect], right: &[Effect]) -> Vec<Effect> {
    sorted(left.iter().chain(right).cloned().collect())
}

fn difference(left: &[Effect], right: &[Effect]) -> Vec<Effect> {
    sorted(
        left.iter()
            .filter(|e| !right.contains(e))
            .cloned()
            .collect(),
    )
}

fn incompatible(left: Vec<Effect>, right: Vec<Effect>) -> EffectRowSolveError {
    EffectRowSolveError::IncompatibleClosedRows {
        left: sorted(left),
        right: sorted(right),
    }
}

impl EffectRowSolver {
    pub fn new() -> Self {
        Self::default()
    }

    /// Resolve a row through transitive tail substitutions; flatten and
    /// normalize its explicitly known effects without losing any open tail.
    pub fn resolve(&self, row: &EffectRow) -> Result<EffectRow, EffectRowSolveError> {
        self.resolve_inner(row, &mut FxHashSet::default())
    }

    fn resolve_inner(
        &self,
        row: &EffectRow,
        visiting: &mut FxHashSet<Region>,
    ) -> Result<EffectRow, EffectRowSolveError> {
        match row {
            EffectRow::Closed(effects) => Ok(EffectRow::Closed(sorted(effects.clone()))),
            EffectRow::Open(effects, tail) => {
                let Some(subst) = self.bindings.get(tail) else {
                    return Ok(EffectRow::Open(sorted(effects.clone()), *tail));
                };
                if !visiting.insert(*tail) {
                    return Err(EffectRowSolveError::CyclicTail(*tail));
                }
                let resolved = self.resolve_inner(subst, visiting);
                visiting.remove(tail);
                match resolved? {
                    EffectRow::Closed(extra) => Ok(EffectRow::Closed(union(effects, &extra))),
                    EffectRow::Open(extra, remaining) => {
                        Ok(EffectRow::Open(union(effects, &extra), remaining))
                    }
                }
            }
        }
    }

    fn atomic(
        &mut self,
        update: impl FnOnce(&mut Self) -> Result<(), EffectRowSolveError>,
    ) -> Result<(), EffectRowSolveError> {
        let mut working = self.clone();
        update(&mut working)?;
        *self = working;
        Ok(())
    }

    /// Bind an unknown tail to a row. Binding to an already constrained
    /// tail unifies with its existing solution, never overwriting it.
    pub fn bind(&mut self, tail: Region, row: EffectRow) -> Result<(), EffectRowSolveError> {
        self.atomic(|solver| solver.bind_inner(tail, row))
    }

    fn bind_inner(&mut self, tail: Region, row: EffectRow) -> Result<(), EffectRowSolveError> {
        if self.bindings.contains_key(&tail) {
            return self.unify_inner(&EffectRow::Open(vec![], tail), &row);
        }
        let resolved = self.resolve(&row)?;
        if matches!(resolved, EffectRow::Open(_, r) if r == tail) {
            return Err(EffectRowSolveError::CyclicTail(tail));
        }
        self.bindings.insert(tail, resolved);
        Ok(())
    }

    /// Solve equality of two effect rows, including residual tail equations.
    ///
    /// This is an *equality constraint*, not a directional effect allowance:
    /// an open row may unify with a closed one by binding its unknown tail,
    /// but the unbound row cannot be treated as pure without that binding
    /// being retained throughout type inference.
    pub fn unify(
        &mut self,
        left: &EffectRow,
        right: &EffectRow,
    ) -> Result<(), EffectRowSolveError> {
        self.atomic(|solver| solver.unify_inner(left, right))
    }

    fn unify_inner(
        &mut self,
        left: &EffectRow,
        right: &EffectRow,
    ) -> Result<(), EffectRowSolveError> {
        let left = self.resolve(left)?;
        let right = self.resolve(right)?;
        match (left, right) {
            (EffectRow::Closed(a), EffectRow::Closed(b)) => {
                if a == b {
                    Ok(())
                } else {
                    Err(incompatible(a, b))
                }
            }
            (EffectRow::Open(known, tail), EffectRow::Closed(all))
            | (EffectRow::Closed(all), EffectRow::Open(known, tail)) => {
                let unmatched = difference(&known, &all);
                if !unmatched.is_empty() {
                    return Err(incompatible(known, all));
                }
                self.bind_inner(tail, EffectRow::Closed(difference(&all, &known)))
            }
            (EffectRow::Open(a, r1), EffectRow::Open(b, r2)) => {
                if r1 == r2 {
                    if a == b {
                        return Ok(());
                    }
                    // Matching tails can solve differences by constraining
                    // their shared value to contain both sets of effects.
                    return self.bind_inner(r1, EffectRow::Open(union(&a, &b), Region::fresh()));
                }

                // {A | r1} = {B | r2}:
                // r1 := {B \\ A | shared}; r2 := {A \\ B | shared}
                // The fresh shared tail preserves all unknown residuals.
                let shared = Region::fresh();
                self.bind_inner(r1, EffectRow::Open(difference(&b, &a), shared))?;
                self.bind_inner(r2, EffectRow::Open(difference(&a, &b), shared))
            }
        }
    }

    /// Apply proven row bindings to every function type nested in a type.
    /// Ordinary HM type variables are left alone; their substitutions are
    /// still maintained by the existing typechecker.
    pub fn apply_to_type(&self, ty: &Type) -> Result<Type, EffectRowSolveError> {
        Ok(match ty {
            Type::Var(_) | Type::Primitive(_) | Type::Skolem(_) => ty.clone(),
            Type::Tuple(items) => Type::Tuple(
                items
                    .iter()
                    .map(|t| self.apply_to_type(t))
                    .collect::<Result<_, _>>()?,
            ),
            Type::Record(fields) => Type::Record(
                fields
                    .iter()
                    .map(|(name, t)| Ok((name.clone(), self.apply_to_type(t)?)))
                    .collect::<Result<_, EffectRowSolveError>>()?,
            ),
            Type::Variant(variants) => Type::Variant(
                variants
                    .iter()
                    .map(|(name, value)| {
                        Ok((
                            name.clone(),
                            value.as_ref().map(|t| self.apply_to_type(t)).transpose()?,
                        ))
                    })
                    .collect::<Result<_, EffectRowSolveError>>()?,
            ),
            Type::Array(inner) => Type::Array(Box::new(self.apply_to_type(inner)?)),
            Type::Function {
                param,
                ret,
                effect,
                cap,
            } => Type::Function {
                param: Box::new(self.apply_to_type(param)?),
                ret: Box::new(self.apply_to_type(ret)?),
                effect: self.resolve(effect)?,
                cap: *cap,
            },
            Type::Actor { state, behavior } => Type::Actor {
                state: Box::new(self.apply_to_type(state)?),
                behavior: Box::new(self.apply_to_type(behavior)?),
            },
            Type::App { constructor, args } => Type::App {
                constructor: Box::new(self.apply_to_type(constructor)?),
                args: args
                    .iter()
                    .map(|t| self.apply_to_type(t))
                    .collect::<Result<_, _>>()?,
            },
            Type::Reference { cap, inner } => Type::Reference {
                cap: *cap,
                inner: Box::new(self.apply_to_type(inner)?),
            },
            Type::Scheme { vars, body } => Type::Scheme {
                vars: vars.clone(),
                body: Box::new(self.apply_to_type(body)?),
            },
            Type::Nominal { name, underlying } => Type::Nominal {
                name: name.clone(),
                underlying: Box::new(self.apply_to_type(underlying)?),
            },
        })
    }
}
