//! Deterministic value-level shrinking for typed DST counterexamples.
//!
//! Candidate generation preserves type shape, never executes production
//! effects, and uses a fresh replay supplied by the caller.

#[cfg(test)]
mod tests {
    use super::*;
    use crate::types::PrimitiveType;

    fn int() -> Type {
        Type::Primitive(PrimitiveType::Int)
    }
    fn string() -> Type {
        Type::Primitive(PrimitiveType::String)
    }
    fn limits() -> GenerationLimits {
        GenerationLimits::default()
    }

    #[test]
    fn integers_shrink_to_a_locally_minimal_failing_value() {
        let input = SimValue::Int(100);
        let result = shrink_failing_value(
            &int(),
            &input,
            &limits(),
            100,
            |value| matches!(value, SimValue::Int(v) if *v >= 10),
        )
        .unwrap();
        assert_eq!(result.value, SimValue::Int(12));
        assert_eq!(result.status, ValueShrinkStatus::LocallyMinimal);
        assert!(result.replays > 1);
        let repeated = shrink_failing_value(
            &int(),
            &input,
            &limits(),
            100,
            |value| matches!(value, SimValue::Int(v) if *v >= 10),
        )
        .unwrap();
        assert_eq!(result, repeated);
    }

    #[test]
    fn typed_record_array_variant_shrinking_preserves_structure() {
        let ty = Type::Record(vec![
            ("count".into(), int()),
            ("tags".into(), Type::Array(Box::new(string()))),
            (
                "choice".into(),
                Type::Variant(vec![
                    ("Some".into(), Some(Type::Primitive(PrimitiveType::Bool))),
                    ("None".into(), None),
                ]),
            ),
        ]);
        let input = SimValue::Record(vec![
            ("count".into(), SimValue::Int(100)),
            (
                "tags".into(),
                SimValue::Array(vec![
                    SimValue::String("hello".into()),
                    SimValue::String("world".into()),
                ]),
            ),
            (
                "choice".into(),
                SimValue::Variant("Some".into(), Some(Box::new(SimValue::Bool(true)))),
            ),
        ]);
        let result = shrink_failing_value(&ty, &input, &limits(), 250, |value| {
            let SimValue::Record(fields) = value else {
                return false;
            };
            let Some((_, SimValue::Int(count))) = fields.iter().find(|(name, _)| name == "count")
            else {
                return false;
            };
            let Some((_, SimValue::Array(tags))) = fields.iter().find(|(name, _)| name == "tags")
            else {
                return false;
            };
            *count >= 10 && !tags.is_empty()
        })
        .unwrap();
        assert_eq!(result.status, ValueShrinkStatus::LocallyMinimal);
        assert!(validate_value(&ty, &result.value, &limits()).is_ok());
        let SimValue::Record(fields) = result.value else {
            panic!("record expected")
        };
        assert_eq!(fields[0].1, SimValue::Int(12));
        assert_eq!(
            fields[1].1,
            SimValue::Array(vec![SimValue::String(String::new())])
        );
        assert_eq!(
            fields[2].1,
            SimValue::Variant("Some".into(), Some(Box::new(SimValue::Bool(false))))
        );
    }

    #[test]
    fn nonreproducing_baseline_is_not_reported_as_minimized() {
        let result =
            shrink_failing_value(&int(), &SimValue::Int(7), &limits(), 30, |_| false).unwrap();
        // The oracle returns true when the *failure* reproduces.
        assert_eq!(result.status, ValueShrinkStatus::NotFailing);
        assert_eq!(result.replays, 1);
        assert_eq!(result.value, SimValue::Int(7));
    }

    #[test]
    fn replay_budget_is_strict_and_never_claims_minimality_on_exhaustion() {
        let mut calls = 0;
        let zero = shrink_failing_value(&int(), &SimValue::Int(50), &limits(), 0, |_| {
            calls += 1;
            true
        })
        .unwrap();
        assert_eq!(zero.replays, 0);
        assert_eq!(calls, 0);
        assert_eq!(zero.status, ValueShrinkStatus::BudgetExhausted);
        let one = shrink_failing_value(&int(), &SimValue::Int(50), &limits(), 1, |_| {
            calls += 1;
            true
        })
        .unwrap();
        assert_eq!(one.replays, 1);
        assert_eq!(one.status, ValueShrinkStatus::BudgetExhausted);
    }

    #[test]
    fn malformed_inputs_fail_without_oracle_invocation() {
        let ty = Type::Record(vec![("balance".into(), int())]);
        let mut calls = 0;
        let value = SimValue::Record(vec![("balance".into(), SimValue::String("42".into()))]);
        assert_eq!(
            shrink_failing_value(&ty, &value, &limits(), 100, |_| {
                calls += 1;
                true
            })
            .unwrap_err(),
            GenerationError::InvalidValue
        );
        assert_eq!(calls, 0);
        let value = SimValue::Record(vec![
            ("balance".into(), SimValue::Int(7)),
            ("extra".into(), SimValue::Int(8)),
        ]);
        assert_eq!(
            shrink_failing_value(&ty, &value, &limits(), 100, |_| {
                calls += 1;
                true
            })
            .unwrap_err(),
            GenerationError::InvalidValue
        );
        assert_eq!(calls, 0);
    }

    #[test]
    fn unicode_string_shrinking_uses_character_boundaries() {
        let value = SimValue::String("Olá🧠".into());
        let shrunk = shrink_failing_value(&string(), &value, &limits(), 100, |_| true).unwrap();
        assert_eq!(shrunk.status, ValueShrinkStatus::LocallyMinimal);
        assert_eq!(shrunk.value, SimValue::String(String::new()));
    }

    #[test]
    fn invariant_that_rejects_all_candidates_is_still_one_minimal() {
        let result = shrink_failing_value(
            &Type::Primitive(PrimitiveType::Bool),
            &SimValue::Bool(true),
            &limits(),
            10,
            |v| matches!(v, SimValue::Bool(true)),
        )
        .unwrap();
        assert_eq!(result.status, ValueShrinkStatus::LocallyMinimal);
        assert_eq!(result.value, SimValue::Bool(true));
    }
}

use crate::dst::generate::{preflight_type, validate_limits, GenerationError, GenerationLimits};
use crate::dst::SimValue;
use crate::types::{PrimitiveType, Type};

/// The claim is relative to the deterministic set of reductions below.
/// It is not a proof of the globally smallest input that reproduces a failure.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ValueShrinkStatus {
    NotFailing,
    LocallyMinimal,
    BudgetExhausted,
}

#[derive(Debug, Clone, PartialEq)]
pub struct ShrunkValue {
    pub value: SimValue,
    /// Includes the initial replay when max_replays is nonzero.
    pub replays: usize,
    pub status: ValueShrinkStatus,
}

/// Validate an already-materialized DST value against a supported compiler
/// type and generation limits; no property callback runs on invalid values.
pub fn validate_value(
    ty: &Type,
    value: &SimValue,
    limits: &GenerationLimits,
) -> Result<(), GenerationError> {
    validate_limits(limits)?;
    preflight_type(ty, limits)?;
    if matches_type(ty, value, limits) {
        Ok(())
    } else {
        Err(GenerationError::InvalidValue)
    }
}

fn matches_type(ty: &Type, value: &SimValue, limits: &GenerationLimits) -> bool {
    match (ty, value) {
        (Type::Primitive(PrimitiveType::Int), SimValue::Int(_))
        | (Type::Primitive(PrimitiveType::Bool), SimValue::Bool(_))
        | (Type::Primitive(PrimitiveType::Nil), SimValue::Nil)
        | (Type::Primitive(PrimitiveType::Unit), SimValue::Unit) => true,
        (Type::Primitive(PrimitiveType::Float), SimValue::Float(n)) => n.is_finite(),
        (Type::Primitive(PrimitiveType::String), SimValue::String(s)) => {
            s.chars().count() <= limits.max_string_len
        }
        (Type::Tuple(types), SimValue::Tuple(items)) => {
            types.len() == items.len()
                && types
                    .iter()
                    .zip(items)
                    .all(|(ty, item)| matches_type(ty, item, limits))
        }
        (Type::Array(ty), SimValue::Array(items)) => {
            items.len() <= limits.max_collection_len
                && items.iter().all(|item| matches_type(ty, item, limits))
        }
        (Type::Record(types), SimValue::Record(fields)) => {
            fields.len() == types.len()
                && types.iter().all(|(name, ty)| {
                    let mut found = fields.iter().filter(|(n, _)| n == name);
                    let Some((_, value)) = found.next() else {
                        return false;
                    };
                    found.next().is_none() && matches_type(ty, value, limits)
                })
        }
        (Type::Variant(tags), SimValue::Variant(name, payload)) => tags.iter().any(|(tag, ty)| {
            tag == name
                && match (ty, payload) {
                    (None, None) => true,
                    (Some(ty), Some(value)) => matches_type(ty, value, limits),
                    _ => false,
                }
        }),
        _ => false,
    }
}

const MAX_CANDIDATES_PER_ROUND: usize = 128;

fn offer(out: &mut Vec<SimValue>, truncated: &mut bool, max: usize, candidate: SimValue) {
    if out.len() < max {
        out.push(candidate);
    } else {
        *truncated = true;
    }
}

/// Deterministically enumerate strictly simplifying, type-preserving
/// candidates. Larger values are intentionally explored only within a bound.
fn candidates(value: &SimValue, max: usize) -> (Vec<SimValue>, bool) {
    let mut out = Vec::new();
    let mut truncated = false;
    match value {
        SimValue::Int(n) if *n != 0 => {
            offer(&mut out, &mut truncated, max, SimValue::Int(0));
            let half = *n / 2;
            if half != 0 {
                offer(&mut out, &mut truncated, max, SimValue::Int(half));
            }
        }
        SimValue::Float(n) if *n != 0.0 => {
            offer(&mut out, &mut truncated, max, SimValue::Float(0.0));
            let half = *n / 2.0;
            if half != 0.0 && half != *n {
                offer(&mut out, &mut truncated, max, SimValue::Float(half));
            }
        }
        SimValue::Bool(true) => offer(&mut out, &mut truncated, max, SimValue::Bool(false)),
        SimValue::String(s) if !s.is_empty() => {
            offer(
                &mut out,
                &mut truncated,
                max,
                SimValue::String(String::new()),
            );
            let chars = s.chars().count();
            if chars > 1 {
                let prefix: String = s.chars().take(chars / 2).collect();
                offer(&mut out, &mut truncated, max, SimValue::String(prefix));
            }
        }
        SimValue::Array(values) => {
            if !values.is_empty() {
                offer(&mut out, &mut truncated, max, SimValue::Array(Vec::new()));
            }
            // Remove each element, then shrink individual elements while
            // leaving every other element in its original order.
            for idx in 0..values.len() {
                if out.len() >= max {
                    truncated = true;
                    break;
                }
                let mut shorter = values.clone();
                shorter.remove(idx);
                offer(&mut out, &mut truncated, max, SimValue::Array(shorter));
            }
            for (idx, value) in values.iter().enumerate() {
                if out.len() >= max {
                    truncated = true;
                    break;
                }
                let (sub, sub_truncated) = candidates(value, max - out.len());
                truncated |= sub_truncated;
                for child in sub {
                    let mut changed = values.clone();
                    changed[idx] = child;
                    offer(&mut out, &mut truncated, max, SimValue::Array(changed));
                }
            }
        }
        SimValue::Tuple(values) => {
            for (idx, value) in values.iter().enumerate() {
                if out.len() >= max {
                    truncated = true;
                    break;
                }
                let (sub, sub_truncated) = candidates(value, max - out.len());
                truncated |= sub_truncated;
                for child in sub {
                    let mut changed = values.clone();
                    changed[idx] = child;
                    offer(&mut out, &mut truncated, max, SimValue::Tuple(changed));
                }
            }
        }
        SimValue::Record(fields) => {
            for (idx, (_, value)) in fields.iter().enumerate() {
                if out.len() >= max {
                    truncated = true;
                    break;
                }
                let (sub, sub_truncated) = candidates(value, max - out.len());
                truncated |= sub_truncated;
                for child in sub {
                    let mut changed = fields.clone();
                    changed[idx].1 = child;
                    offer(&mut out, &mut truncated, max, SimValue::Record(changed));
                }
            }
        }
        SimValue::Variant(name, Some(value)) => {
            let (sub, sub_truncated) = candidates(value, max);
            truncated |= sub_truncated;
            for child in sub {
                offer(
                    &mut out,
                    &mut truncated,
                    max,
                    SimValue::Variant(name.clone(), Some(Box::new(child))),
                );
            }
        }
        _ => {}
    }
    (out, truncated)
}

/// Minimize a typed counterexample while preserving the caller's failure
/// predicate. Each attempt must rerun the property on fresh deterministic
/// state. `LocallyMinimal` is only established when all offered one-step
/// reductions are tested without reproducing the failure.
///
/// The reduction strategy covers scalar zero/halving, string shortening,
/// array removal, and recursive reductions of tuple/record/variant payloads.
/// It does not switch variant tags, shrink actor topology, or cross effect
/// boundaries. Those need separate type-aware strategies.
pub fn shrink_failing_value<F: FnMut(&SimValue) -> bool>(
    ty: &Type,
    original: &SimValue,
    limits: &GenerationLimits,
    max_replays: usize,
    mut still_fails: F,
) -> Result<ShrunkValue, GenerationError> {
    validate_value(ty, original, limits)?;
    let mut current = original.clone();
    if max_replays == 0 {
        return Ok(ShrunkValue {
            value: current,
            replays: 0,
            status: ValueShrinkStatus::BudgetExhausted,
        });
    }
    let mut replays = 1;
    if !still_fails(&current) {
        return Ok(ShrunkValue {
            value: current,
            replays,
            status: ValueShrinkStatus::NotFailing,
        });
    }
    loop {
        let (alternatives, truncated) = candidates(&current, MAX_CANDIDATES_PER_ROUND);
        let mut progressed = false;
        for next in alternatives {
            if replays >= max_replays {
                return Ok(ShrunkValue {
                    value: current,
                    replays,
                    status: ValueShrinkStatus::BudgetExhausted,
                });
            }
            replays += 1;
            if still_fails(&next) {
                current = next;
                progressed = true;
                break;
            }
        }
        if !progressed {
            return Ok(ShrunkValue {
                value: current,
                replays,
                status: if truncated {
                    ValueShrinkStatus::BudgetExhausted
                } else {
                    ValueShrinkStatus::LocallyMinimal
                },
            });
        }
    }
}
