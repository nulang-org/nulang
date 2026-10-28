//! Declarative Rust-side contracts on compiler types for deterministic DST.
//! Not a Nulang source-language contract or runtime actor admission mechanism.

#[cfg(test)]
mod tests {
    use super::*;
    use crate::types::{PrimitiveType, TypeVar};

    fn int() -> Type {
        Type::Primitive(PrimitiveType::Int)
    }
    fn contract(name: &str) -> TypedPropertyContract {
        TypedPropertyContract {
            name: name.into(),
            input_type: int(),
            seed: 123,
            trials: 24,
            limits: GenerationLimits::default(),
            shrink_replay_budget: 100,
        }
    }

    #[test]
    fn valid_typed_contract_passes_all_trials() {
        let result = run_typed_contract(&contract("bank.balance.is_integer"), |sample| {
            matches!(sample, SimValue::Int(_))
        })
        .unwrap();
        assert_eq!(
            result,
            ContractOutcome::Passed {
                name: "bank.balance.is_integer".into(),
                seed: 123,
                trials: 24
            }
        );
    }

    #[test]
    fn failing_contract_captures_original_and_reproducibly_shrinks() {
        let descriptor = contract("bank.transfer.never_negative");
        let first = run_typed_contract(&descriptor, |_| false).unwrap();
        let repeated = run_typed_contract(&descriptor, |_| false).unwrap();
        assert_eq!(first, repeated);
        let ContractOutcome::Failed {
            name,
            counterexample,
            shrunk,
        } = first
        else {
            panic!("expected failure")
        };
        assert_eq!(name, "bank.transfer.never_negative");
        assert_eq!(counterexample.trial, 0);
        assert_eq!(counterexample.seed, 123);
        assert_eq!(shrunk.status, ValueShrinkStatus::LocallyMinimal);
        assert!(matches!(shrunk.value, SimValue::Int(0)));
    }

    #[test]
    fn invalid_contract_metadata_fails_without_executing_predicate() {
        let mut descriptor = contract(" ");
        let mut calls = 0;
        assert_eq!(
            run_typed_contract(&descriptor, |_| {
                calls += 1;
                true
            })
            .unwrap_err(),
            ContractError::InvalidName
        );
        descriptor.name = "valid".into();
        descriptor.trials = 0;
        assert_eq!(
            run_typed_contract(&descriptor, |_| {
                calls += 1;
                true
            })
            .unwrap_err(),
            ContractError::Generation(GenerationError::InvalidTrials)
        );
        assert_eq!(calls, 0);
        descriptor.trials = 1;
        descriptor.input_type = Type::Var(TypeVar(42));
        assert!(matches!(
            run_typed_contract(&descriptor, |_| {
                calls += 1;
                true
            }),
            Err(ContractError::Generation(GenerationError::Unsupported(_)))
        ));
        assert_eq!(calls, 0);
    }

    #[test]
    fn stateful_predicate_is_rejected_when_failure_does_not_reproduce() {
        let mut calls = 0;
        let result = run_typed_contract(&contract("unstable"), |_| {
            calls += 1;
            calls != 1
        });
        assert_eq!(result.unwrap_err(), ContractError::NotReproducible);
    }

    #[test]
    fn replay_budget_exhaustion_is_visible_in_contract_result() {
        let mut descriptor = contract("exhausted");
        descriptor.shrink_replay_budget = 0;
        let result = run_typed_contract(&descriptor, |_| false).unwrap();
        let ContractOutcome::Failed { shrunk, .. } = result else {
            panic!("expected failure")
        };
        assert_eq!(shrunk.replays, 0);
        assert_eq!(shrunk.status, ValueShrinkStatus::BudgetExhausted);
    }
}

use crate::dst::generate::{
    check_property, GenerationError, GenerationLimits, PropertyCounterexample,
};
use crate::dst::value_shrink::{shrink_failing_value, ShrunkValue, ValueShrinkStatus};
use crate::dst::SimValue;
use crate::types::Type;

/// Opt-in Rust-side contract metadata. No Nulang parser or runtime semantics
/// depend on this descriptor, and no global contract registry is created.
#[derive(Debug, Clone)]
pub struct TypedPropertyContract {
    pub name: String,
    pub input_type: Type,
    pub seed: u64,
    pub trials: usize,
    pub limits: GenerationLimits,
    pub shrink_replay_budget: usize,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ContractError {
    InvalidName,
    Generation(GenerationError),
    /// The predicate returned inconsistent outcomes for the same sample.
    /// A generated-property predicate must be pure and deterministic.
    NotReproducible,
}

impl From<GenerationError> for ContractError {
    fn from(value: GenerationError) -> Self {
        Self::Generation(value)
    }
}

impl std::fmt::Display for ContractError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "{self:?}")
    }
}

impl std::error::Error for ContractError {}

/// The first failing sample is retained even if shrinking runs out of budget.
#[derive(Debug, Clone, PartialEq)]
pub enum ContractOutcome {
    Passed {
        name: String,
        seed: u64,
        trials: usize,
    },
    Failed {
        name: String,
        counterexample: PropertyCounterexample,
        shrunk: ShrunkValue,
    },
}

/// Run a named, typed, opt-in property contract using existing DST primitives.
/// `property` must be a pure predicate; `true` means the invariant holds.
/// A reproducing failure is passed to the value shrinker, whose callback
/// inversely reports `true` when the same invariant still fails.
pub fn run_typed_contract<F>(
    contract: &TypedPropertyContract,
    mut property: F,
) -> Result<ContractOutcome, ContractError>
where
    F: FnMut(&SimValue) -> bool,
{
    if contract.name.trim().is_empty()
        || contract.name.len() > 128
        || contract.name.trim() != contract.name
    {
        return Err(ContractError::InvalidName);
    }

    let failure = check_property(
        &contract.input_type,
        contract.seed,
        contract.trials,
        &contract.limits,
        |value| property(value),
    )?;

    let Some(counterexample) = failure else {
        return Ok(ContractOutcome::Passed {
            name: contract.name.clone(),
            seed: contract.seed,
            trials: contract.trials,
        });
    };

    let shrunk = shrink_failing_value(
        &contract.input_type,
        &counterexample.value,
        &contract.limits,
        contract.shrink_replay_budget,
        |value| !property(value),
    )?;

    if shrunk.status == ValueShrinkStatus::NotFailing {
        return Err(ContractError::NotReproducible);
    }
    Ok(ContractOutcome::Failed {
        name: contract.name.clone(),
        counterexample,
        shrunk,
    })
}
