//! Conservative admission policy for actor message interleaving.
//!
//! Nulang's default remains one actor turn at a time. General reentrancy is not
//! introduced here: it can expose intermediate mutable state and make durable
//! replay timing-dependent. This module models only the narrow future case that
//! can be made safe: compiler-verified read-only behaviors may run while an
//! explicitly interleavable turn is suspended.

use serde::{Deserialize, Serialize};
use std::fmt;

/// Compiler/runtime classification of a behavior turn.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum BehaviorConcurrency {
    /// May mutate actor state or perform effects; owns exclusive actor access.
    Exclusive,
    /// Proven not to mutate actor state and restricted to replay-safe effects.
    /// This classification should eventually be emitted by the compiler, not
    /// trusted from arbitrary user metadata.
    ReadOnly,
}

/// What a suspended exclusive turn permits the scheduler to admit behind it.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum SuspensionInterleave {
    /// Current Nulang semantics: queued messages remain blocked.
    BlockAll,
    /// A compiler/runtime-certified suspension point permits read-only queries.
    AllowReadOnly,
}

/// Actor-local concurrency policy. `max_read_only_in_flight == 0` is strict
/// serial mode and is the default for compatibility.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub struct ActorInterleavingPolicy {
    pub max_read_only_in_flight: u16,
}

impl ActorInterleavingPolicy {
    pub const fn serial() -> Self {
        Self {
            max_read_only_in_flight: 0,
        }
    }

    pub fn read_only(max_in_flight: u16) -> Result<Self, InterleavingPolicyError> {
        if max_in_flight == 0 {
            return Err(InterleavingPolicyError::ZeroReadOnlyLimit);
        }
        Ok(Self {
            max_read_only_in_flight: max_in_flight,
        })
    }
}

impl Default for ActorInterleavingPolicy {
    fn default() -> Self {
        Self::serial()
    }
}

/// Minimal scheduler-visible actor turn state.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub struct TurnAdmissionState {
    /// An exclusive turn currently owns the actor. If suspended, `suspension`
    /// describes whether read-only interleaving was explicitly allowed.
    pub exclusive_active: bool,
    pub suspension: Option<SuspensionInterleave>,
    pub read_only_in_flight: u16,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum AdmissionDecision {
    /// Normal serial admission when no other turn owns the actor.
    AdmitExclusive,
    /// Read-only turn admitted without another active turn.
    AdmitReadOnly,
    /// Read-only turn admitted behind an explicitly interleavable suspension.
    AdmitInterleavedReadOnly,
    DenySerialPolicy,
    DenyExclusiveBusy,
    DenySuspensionBlocks,
    DenyReadOnlyLimit,
}

/// Decide whether a queued behavior may start without mutating scheduler state.
pub fn admission_decision(
    policy: ActorInterleavingPolicy,
    state: TurnAdmissionState,
    incoming: BehaviorConcurrency,
) -> AdmissionDecision {
    if policy.max_read_only_in_flight == 0 {
        if state.exclusive_active || state.read_only_in_flight != 0 {
            return AdmissionDecision::DenySerialPolicy;
        }
        return match incoming {
            BehaviorConcurrency::Exclusive => AdmissionDecision::AdmitExclusive,
            BehaviorConcurrency::ReadOnly => AdmissionDecision::AdmitReadOnly,
        };
    }

    if state.exclusive_active {
        return match state.suspension {
            None => AdmissionDecision::DenyExclusiveBusy,
            Some(SuspensionInterleave::BlockAll) => AdmissionDecision::DenySuspensionBlocks,
            Some(SuspensionInterleave::AllowReadOnly) => match incoming {
                BehaviorConcurrency::Exclusive => AdmissionDecision::DenyExclusiveBusy,
                BehaviorConcurrency::ReadOnly => {
                    if state.read_only_in_flight >= policy.max_read_only_in_flight {
                        AdmissionDecision::DenyReadOnlyLimit
                    } else {
                        AdmissionDecision::AdmitInterleavedReadOnly
                    }
                }
            },
        };
    }

    if state.read_only_in_flight != 0 {
        return match incoming {
            BehaviorConcurrency::Exclusive => AdmissionDecision::DenyExclusiveBusy,
            BehaviorConcurrency::ReadOnly => {
                if state.read_only_in_flight >= policy.max_read_only_in_flight {
                    AdmissionDecision::DenyReadOnlyLimit
                } else {
                    AdmissionDecision::AdmitReadOnly
                }
            }
        };
    }

    match incoming {
        BehaviorConcurrency::Exclusive => AdmissionDecision::AdmitExclusive,
        BehaviorConcurrency::ReadOnly => AdmissionDecision::AdmitReadOnly,
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum InterleavingPolicyError {
    ZeroReadOnlyLimit,
}

impl fmt::Display for InterleavingPolicyError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::ZeroReadOnlyLimit => write!(
                f,
                "read-only interleaving requires max_in_flight > 0; use serial() to disable it"
            ),
        }
    }
}

impl std::error::Error for InterleavingPolicyError {}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn default_policy_preserves_current_serial_semantics() {
        let policy = ActorInterleavingPolicy::default();
        assert_eq!(
            admission_decision(policy, TurnAdmissionState::default(), BehaviorConcurrency::Exclusive),
            AdmissionDecision::AdmitExclusive
        );
        assert_eq!(
            admission_decision(
                policy,
                TurnAdmissionState {
                    exclusive_active: true,
                    suspension: Some(SuspensionInterleave::AllowReadOnly),
                    read_only_in_flight: 0,
                },
                BehaviorConcurrency::ReadOnly,
            ),
            AdmissionDecision::DenySerialPolicy
        );
    }

    #[test]
    fn read_only_may_interleave_only_at_explicit_safe_suspension() {
        let policy = ActorInterleavingPolicy::read_only(4).unwrap();
        let state = TurnAdmissionState {
            exclusive_active: true,
            suspension: Some(SuspensionInterleave::AllowReadOnly),
            read_only_in_flight: 0,
        };
        assert_eq!(
            admission_decision(policy, state, BehaviorConcurrency::ReadOnly),
            AdmissionDecision::AdmitInterleavedReadOnly
        );
        assert_eq!(
            admission_decision(policy, state, BehaviorConcurrency::Exclusive),
            AdmissionDecision::DenyExclusiveBusy
        );
    }

    #[test]
    fn ordinary_suspension_continues_to_block_mailbox() {
        let policy = ActorInterleavingPolicy::read_only(4).unwrap();
        let state = TurnAdmissionState {
            exclusive_active: true,
            suspension: Some(SuspensionInterleave::BlockAll),
            read_only_in_flight: 0,
        };
        assert_eq!(
            admission_decision(policy, state, BehaviorConcurrency::ReadOnly),
            AdmissionDecision::DenySuspensionBlocks
        );
    }

    #[test]
    fn running_exclusive_turn_never_interleaves() {
        let policy = ActorInterleavingPolicy::read_only(4).unwrap();
        let state = TurnAdmissionState {
            exclusive_active: true,
            suspension: None,
            read_only_in_flight: 0,
        };
        assert_eq!(
            admission_decision(policy, state, BehaviorConcurrency::ReadOnly),
            AdmissionDecision::DenyExclusiveBusy
        );
    }

    #[test]
    fn read_only_limit_is_enforced() {
        let policy = ActorInterleavingPolicy::read_only(2).unwrap();
        let state = TurnAdmissionState {
            exclusive_active: true,
            suspension: Some(SuspensionInterleave::AllowReadOnly),
            read_only_in_flight: 2,
        };
        assert_eq!(
            admission_decision(policy, state, BehaviorConcurrency::ReadOnly),
            AdmissionDecision::DenyReadOnlyLimit
        );
    }

    #[test]
    fn exclusive_waits_for_active_readers() {
        let policy = ActorInterleavingPolicy::read_only(4).unwrap();
        let state = TurnAdmissionState {
            exclusive_active: false,
            suspension: None,
            read_only_in_flight: 1,
        };
        assert_eq!(
            admission_decision(policy, state, BehaviorConcurrency::Exclusive),
            AdmissionDecision::DenyExclusiveBusy
        );
    }

    #[test]
    fn zero_limit_must_use_explicit_serial_constructor() {
        assert_eq!(
            ActorInterleavingPolicy::read_only(0),
            Err(InterleavingPolicyError::ZeroReadOnlyLimit)
        );
    }
}
