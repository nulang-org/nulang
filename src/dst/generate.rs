//! Deterministic, bounded generation of DST inputs from Nulang types.
//!
//! This is an opt-in test utility, not a production runtime or source-language
//! contract feature. Unsupported types fail closed rather than producing
//! incorrectly typed messages.

#[cfg(test)]
mod tests {
    use super::*;
    use crate::dst::shrink::{minimize_failing_trace, MinimizationStatus};

    fn int() -> Type {
        Type::Primitive(PrimitiveType::Int)
    }

    #[test]
    fn same_seed_produces_the_same_values() {
        let ty = Type::Tuple(vec![int(), Type::Primitive(PrimitiveType::String)]);
        let mut first = DeterministicRng::new(421);
        let mut second = DeterministicRng::new(421);
        let limits = GenerationLimits::default();
        let a: Vec<_> = (0..32)
            .map(|_| generate_value(&ty, &mut first, &limits).unwrap())
            .collect();
        let b: Vec<_> = (0..32)
            .map(|_| generate_value(&ty, &mut second, &limits).unwrap())
            .collect();
        assert_eq!(a, b);
    }

    #[test]
    fn records_have_canonical_field_order_and_typed_nested_values() {
        let first = Type::Record(vec![
            ("z".into(), Type::Array(Box::new(int()))),
            ("a".into(), Type::Primitive(PrimitiveType::Bool)),
        ]);
        let second = Type::Record(vec![
            ("a".into(), Type::Primitive(PrimitiveType::Bool)),
            ("z".into(), Type::Array(Box::new(int()))),
        ]);
        let limits = GenerationLimits {
            max_depth: 6,
            max_collection_len: 3,
            max_string_len: 4,
        };
        let mut a = DeterministicRng::new(17);
        let mut b = DeterministicRng::new(17);
        for _ in 0..24 {
            let generated = generate_value(&first, &mut a, &limits).unwrap();
            assert_eq!(generated, generate_value(&second, &mut b, &limits).unwrap());
            match generated {
                SimValue::Record(fields) => {
                    assert_eq!(fields.len(), 2);
                    assert_eq!(fields[0].0, "a");
                    assert_eq!(fields[1].0, "z");
                    assert!(matches!(fields[0].1, SimValue::Bool(_)));
                    assert!(
                        matches!(&fields[1].1, SimValue::Array(items) if items.len() <= 3 && items.iter().all(|item| matches!(item, SimValue::Int(_))))
                    );
                }
                other => panic!("unexpected value: {other:?}"),
            }
        }
    }

    #[test]
    fn variants_only_select_declared_tags_and_payload_types() {
        let ty = Type::Variant(vec![("None".into(), None), ("Some".into(), Some(int()))]);
        let mut rng = DeterministicRng::new(99);
        let mut saw_none = false;
        let mut saw_some = false;
        for _ in 0..64 {
            match generate_value(&ty, &mut rng, &GenerationLimits::default()).unwrap() {
                SimValue::Variant(tag, None) if tag == "None" => saw_none = true,
                SimValue::Variant(tag, Some(value))
                    if tag == "Some" && matches!(*value, SimValue::Int(_)) =>
                {
                    saw_some = true
                }
                other => panic!("invalid variant: {other:?}"),
            }
        }
        assert!(saw_none && saw_some);
    }

    #[test]
    fn bounds_are_enforced_without_silent_type_substitution() {
        let limits = GenerationLimits {
            max_depth: 2,
            max_collection_len: 2,
            max_string_len: 3,
        };
        let mut rng = DeterministicRng::new(4);
        for _ in 0..50 {
            let value = generate_value(&Type::Array(Box::new(int())), &mut rng, &limits).unwrap();
            assert!(matches!(value, SimValue::Array(v) if v.len() <= 2));
            let value =
                generate_value(&Type::Primitive(PrimitiveType::String), &mut rng, &limits).unwrap();
            assert!(matches!(value, SimValue::String(s) if s.len() <= 3 && s.is_ascii()));
        }
        let nested = Type::Array(Box::new(Type::Array(Box::new(int()))));
        assert_eq!(
            generate_value(
                &nested,
                &mut rng,
                &GenerationLimits {
                    max_depth: 1,
                    ..limits
                }
            )
            .unwrap_err(),
            GenerationError::DepthLimit
        );
    }

    #[test]
    fn reject_uninhabited_unresolved_unsupported_and_ambiguous_types() {
        let limits = GenerationLimits::default();
        let mut rng = DeterministicRng::new(1);
        assert_eq!(
            generate_value(&Type::Primitive(PrimitiveType::Never), &mut rng, &limits).unwrap_err(),
            GenerationError::Uninhabited
        );
        assert_eq!(
            generate_value(&Type::Variant(vec![]), &mut rng, &limits).unwrap_err(),
            GenerationError::Uninhabited
        );
        assert!(matches!(
            generate_value(&Type::Var(crate::types::TypeVar(7)), &mut rng, &limits),
            Err(GenerationError::Unsupported(_))
        ));
        assert!(matches!(
            generate_value(
                &Type::Reference {
                    cap: crate::types::Capability::Ref,
                    inner: Box::new(int())
                },
                &mut rng,
                &limits
            ),
            Err(GenerationError::Unsupported(_))
        ));
        assert_eq!(
            generate_value(
                &Type::Record(vec![("a".into(), int()), ("a".into(), int())]),
                &mut rng,
                &limits
            )
            .unwrap_err(),
            GenerationError::DuplicateField("a".into())
        );
        assert_eq!(
            generate_value(
                &int(),
                &mut rng,
                &GenerationLimits {
                    max_depth: 0,
                    ..limits
                }
            )
            .unwrap_err(),
            GenerationError::InvalidLimits
        );
    }

    #[test]
    fn generates_payloads_from_compiler_types_for_dst_messages() {
        let spec = MessageSpec {
            sender: 1,
            target: 2,
            behavior: "debit".into(),
            parameters: vec![int(), Type::Primitive(PrimitiveType::Bool)],
        };
        let mut rng = DeterministicRng::new(42);
        let message = generate_message(&spec, &mut rng, &GenerationLimits::default()).unwrap();
        assert_eq!(
            (message.sender, message.target, message.behavior.as_str()),
            (1, 2, "debit")
        );
        assert!(matches!(
            message.payload.as_slice(),
            [SimValue::Int(_), SimValue::Bool(_)]
        ));
    }

    #[test]
    fn property_failure_is_reproducible_from_seed_and_trial() {
        let limits = GenerationLimits::default();
        let ty = int();
        let failed = check_property(&ty, 11, 25, &limits, |_| false)
            .unwrap()
            .unwrap();
        assert_eq!(failed.trial, 0);
        let repeated = check_property(&ty, 11, 25, &limits, |_| false)
            .unwrap()
            .unwrap();
        assert_eq!(failed, repeated);
        assert!(check_property(&ty, 11, 25, &limits, |_| true)
            .unwrap()
            .is_none());
    }

    #[test]
    fn rejects_explosive_composites_before_consuming_randomness() {
        let limits = GenerationLimits {
            max_depth: 3,
            max_collection_len: 100,
            max_string_len: 3,
        };
        let type_of_nested_arrays = Type::Array(Box::new(Type::Array(Box::new(int()))));
        let mut rng = DeterministicRng::new(15);
        let mut control = rng.clone();
        assert_eq!(
            generate_value(&type_of_nested_arrays, &mut rng, &limits).unwrap_err(),
            GenerationError::ValueBudget
        );
        assert_eq!(rng.next(), control.next());

        let spec = MessageSpec {
            sender: 0,
            target: 1,
            behavior: "many".into(),
            parameters: vec![int(); 4_100],
        };
        assert_eq!(
            generate_message(&spec, &mut rng, &GenerationLimits::default()).unwrap_err(),
            GenerationError::ValueBudget
        );
        assert_eq!(rng.next(), control.next());
    }

    #[test]
    fn property_checker_rejects_zero_trials() {
        assert_eq!(
            check_property(&int(), 1, 0, &GenerationLimits::default(), |_| true).unwrap_err(),
            GenerationError::InvalidTrials
        );
    }

    #[test]
    fn generated_message_sequences_work_with_existing_dst_shrinker() {
        let spec = MessageSpec {
            sender: 0,
            target: 1,
            behavior: "event".into(),
            parameters: vec![int()],
        };
        let mut rng = DeterministicRng::new(7);
        let mut messages = Vec::new();
        for _ in 0..12 {
            messages.push(generate_message(&spec, &mut rng, &GenerationLimits::default()).unwrap());
        }
        let failure = |trace: &[SimMessage]| trace.len() >= 3;
        let result = minimize_failing_trace(&messages, 100, failure);
        assert_eq!(result.status, MinimizationStatus::OneMinimal);
        assert_eq!(result.trace.len(), 3);
    }
}

use crate::dst::{DeterministicRng, SimMessage, SimValue};
use crate::types::{PrimitiveType, Type};

/// Resource limits for generated test inputs. Nested types fail instead of
/// being silently replaced by empty or incorrectly typed values.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct GenerationLimits {
    pub max_depth: usize,
    pub max_collection_len: usize,
    pub max_string_len: usize,
}

impl Default for GenerationLimits {
    fn default() -> Self {
        Self {
            max_depth: 8,
            max_collection_len: 4,
            max_string_len: 16,
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum GenerationError {
    InvalidLimits,
    InvalidTrials,
    InvalidValue,
    ValueBudget,
    DepthLimit,
    Uninhabited,
    Unsupported(String),
    DuplicateField(String),
    DuplicateTag(String),
}

impl std::fmt::Display for GenerationError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "{self:?}")
    }
}

impl std::error::Error for GenerationError {}

/// A type-checked message shape suitable for the existing DST simulator.
/// This metadata is test-only and is not an actor protocol admission check.
#[derive(Debug, Clone)]
pub struct MessageSpec {
    pub sender: u64,
    pub target: u64,
    pub behavior: String,
    pub parameters: Vec<Type>,
}

/// The first generated sample that violates a caller-provided invariant.
#[derive(Debug, Clone, PartialEq)]
pub struct PropertyCounterexample {
    pub seed: u64,
    /// Zero-based trial index; replay uses the same seed and generator limits.
    pub trial: usize,
    pub value: SimValue,
}

pub(super) fn validate_limits(limits: &GenerationLimits) -> Result<(), GenerationError> {
    // Cap both recursion and allocations. A zero collection/string limit
    // intentionally permits only empty values.
    if limits.max_depth == 0
        || limits.max_depth > 64
        || limits.max_collection_len > 256
        || limits.max_string_len > 256
    {
        return Err(GenerationError::InvalidLimits);
    }
    Ok(())
}

fn validate_type(ty: &Type, depth: usize, max_depth: usize) -> Result<(), GenerationError> {
    if depth > max_depth {
        return Err(GenerationError::DepthLimit);
    }
    match ty {
        Type::Primitive(PrimitiveType::Never) => Err(GenerationError::Uninhabited),
        Type::Primitive(PrimitiveType::Address) => Err(GenerationError::Unsupported(
            "actor addresses require a registered target".into(),
        )),
        Type::Primitive(_) => Ok(()),
        Type::Tuple(items) => {
            for item in items {
                validate_type(item, depth + 1, max_depth)?;
            }
            Ok(())
        }
        Type::Array(item) => validate_type(item, depth + 1, max_depth),
        Type::Record(fields) => {
            let mut names = std::collections::HashSet::new();
            for (name, field_type) in fields {
                if !names.insert(name.as_str()) {
                    return Err(GenerationError::DuplicateField(name.clone()));
                }
                validate_type(field_type, depth + 1, max_depth)?;
            }
            Ok(())
        }
        Type::Variant(tags) => {
            if tags.is_empty() {
                return Err(GenerationError::Uninhabited);
            }
            let mut names = std::collections::HashSet::new();
            for (name, payload) in tags {
                if !names.insert(name.as_str()) {
                    return Err(GenerationError::DuplicateTag(name.clone()));
                }
                if let Some(ty) = payload {
                    validate_type(ty, depth + 1, max_depth)?;
                }
            }
            Ok(())
        }
        // Nominals, references, generics, actors and functions require
        // additional compiler/runtime witnesses; never erase that identity
        // and pretend that a raw DST value satisfies the type.
        other => Err(GenerationError::Unsupported(other.to_string())),
    }
}

// Conservatively estimate the worst-case number of materialized DST nodes,
// including every possible element of a nested array. The cap prevents a
// caller-provided shape from expanding exponentially before any allocation.
const MAX_GENERATED_NODES: usize = 4_096;

fn worst_case_nodes(ty: &Type, max_len: usize) -> usize {
    match ty {
        Type::Array(item) => {
            1usize.saturating_add(max_len.saturating_mul(worst_case_nodes(item, max_len)))
        }
        Type::Tuple(items) => items.iter().fold(1usize, |sum, item| {
            sum.saturating_add(worst_case_nodes(item, max_len))
        }),
        Type::Record(fields) => fields.iter().fold(1usize, |sum, (_, field)| {
            sum.saturating_add(worst_case_nodes(field, max_len))
        }),
        Type::Variant(variants) => 1usize.saturating_add(
            variants
                .iter()
                .filter_map(|(_, payload)| payload.as_ref())
                .map(|item| worst_case_nodes(item, max_len))
                .max()
                .unwrap_or(0),
        ),
        _ => 1,
    }
}

pub(super) fn preflight_type(
    ty: &Type,
    limits: &GenerationLimits,
) -> Result<usize, GenerationError> {
    validate_type(ty, 0, limits.max_depth)?;
    let nodes = worst_case_nodes(ty, limits.max_collection_len);
    if nodes > MAX_GENERATED_NODES {
        Err(GenerationError::ValueBudget)
    } else {
        Ok(nodes)
    }
}

fn generate_checked(ty: &Type, rng: &mut DeterministicRng, limits: &GenerationLimits) -> SimValue {
    match ty {
        Type::Primitive(p) => match p {
            PrimitiveType::Int => {
                let n = match rng.next() % 6 {
                    0 => 0,
                    1 => 1,
                    2 => -1,
                    3 => i64::MIN,
                    4 => i64::MAX,
                    _ => rng.next() as i64,
                };
                SimValue::Int(n)
            }
            PrimitiveType::Float => {
                let value = match rng.next() % 5 {
                    0 => 0.0,
                    1 => 1.0,
                    2 => -1.0,
                    3 => f64::MIN_POSITIVE,
                    _ => (rng.next() as i64 as f64) / 1_000_000.0,
                };
                SimValue::Float(value)
            }
            PrimitiveType::Bool => SimValue::Bool(rng.next() & 1 == 0),
            PrimitiveType::String => {
                const ALPHABET: &[u8] = b"abcdefghijklmnopqrstuvwxyz0123456789";
                let len = (rng.next() as usize) % (limits.max_string_len + 1);
                let mut text = String::with_capacity(len);
                for _ in 0..len {
                    text.push(ALPHABET[(rng.next() as usize) % ALPHABET.len()] as char);
                }
                SimValue::String(text)
            }
            PrimitiveType::Nil => SimValue::Nil,
            PrimitiveType::Unit => SimValue::Unit,
            PrimitiveType::Never | PrimitiveType::Address => {
                unreachable!("validated before generation")
            }
        },
        Type::Tuple(items) => SimValue::Tuple(
            items
                .iter()
                .map(|t| generate_checked(t, rng, limits))
                .collect(),
        ),
        Type::Array(item) => {
            let len = (rng.next() as usize) % (limits.max_collection_len + 1);
            SimValue::Array(
                (0..len)
                    .map(|_| generate_checked(item, rng, limits))
                    .collect(),
            )
        }
        Type::Record(fields) => {
            // Closed record field order is not semantically significant.
            let mut sorted: Vec<_> = fields.iter().collect();
            sorted.sort_by(|a, b| a.0.cmp(&b.0));
            SimValue::Record(
                sorted
                    .into_iter()
                    .map(|(name, ty)| (name.clone(), generate_checked(ty, rng, limits)))
                    .collect(),
            )
        }
        Type::Variant(tags) => {
            let mut sorted: Vec<_> = tags.iter().collect();
            sorted.sort_by(|a, b| a.0.cmp(&b.0));
            let selected = sorted[(rng.next() as usize) % sorted.len()];
            let payload = selected
                .1
                .as_ref()
                .map(|t| Box::new(generate_checked(t, rng, limits)));
            SimValue::Variant(selected.0.clone(), payload)
        }
        _ => unreachable!("validated before generation"),
    }
}

/// Generate a value conforming to the supported *closed* compiler type subset.
/// The caller supplies a DST seed and can reproduce the exact sequence.
/// No production evaluation, mutation, IO, or side effects occur here.
pub fn generate_value(
    ty: &Type,
    rng: &mut DeterministicRng,
    limits: &GenerationLimits,
) -> Result<SimValue, GenerationError> {
    validate_limits(limits)?;
    preflight_type(ty, limits)?;
    Ok(generate_checked(ty, rng, limits))
}

/// Generate a typed test message. All parameter types are checked before
/// consuming any RNG state, so unsupported messages never partially generate.
pub fn generate_message(
    spec: &MessageSpec,
    rng: &mut DeterministicRng,
    limits: &GenerationLimits,
) -> Result<SimMessage, GenerationError> {
    validate_limits(limits)?;
    let mut total_nodes = 0usize;
    for param in &spec.parameters {
        total_nodes = total_nodes.saturating_add(preflight_type(param, limits)?);
        if total_nodes > MAX_GENERATED_NODES {
            return Err(GenerationError::ValueBudget);
        }
    }
    Ok(SimMessage {
        sender: spec.sender,
        target: spec.target,
        behavior: spec.behavior.clone(),
        payload: spec
            .parameters
            .iter()
            .map(|param| generate_checked(param, rng, limits))
            .collect(),
    })
}

/// Exercise a pure predicate on deterministic valid samples. The predicate
/// returns true for success; the first false value is retained for replay.
/// For stateful actor properties, use fresh simulator instances per trial and
/// reuse `dst::shrink::minimize_failing_trace` on generated message sequences.
pub fn check_property<F: FnMut(&SimValue) -> bool>(
    ty: &Type,
    seed: u64,
    trials: usize,
    limits: &GenerationLimits,
    mut property: F,
) -> Result<Option<PropertyCounterexample>, GenerationError> {
    if trials == 0 {
        return Err(GenerationError::InvalidTrials);
    }
    validate_limits(limits)?;
    preflight_type(ty, limits)?;
    let mut rng = DeterministicRng::new(seed);
    for trial in 0..trials {
        let value = generate_checked(ty, &mut rng, limits);
        if !property(&value) {
            return Ok(Some(PropertyCounterexample { seed, trial, value }));
        }
    }
    Ok(None)
}
