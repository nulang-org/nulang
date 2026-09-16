//! Durable activation fencing contract for non-CRDT actor state.
//!
//! Routing/directory epochs are not sufficient to prevent stale storage writes:
//! authority can change between an in-memory check and a persistence commit.
//! Backends use this module's decision semantics *inside* the same atomic
//! transaction/write boundary that mutates actor state.

/// Epoch 1 is the initial authoritative activation in RFC 0014.
pub const INITIAL_ACTIVATION_EPOCH: u64 = 1;

/// Fencing token presented by one actor activation when committing durable
/// non-CRDT state.
#[derive(Debug, Clone, Copy, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
pub struct ActivationFence {
    pub actor_id: u64,
    pub epoch: u64,
}

impl ActivationFence {
    pub fn new(actor_id: u64, epoch: u64) -> Result<Self, ActivationFenceError> {
        if epoch == 0 {
            return Err(ActivationFenceError::ZeroEpoch { actor_id });
        }
        Ok(Self { actor_id, epoch })
    }

    /// Uniform single-node policy for durable actors that never opt into
    /// failover. They still write under an explicit epoch rather than bypassing
    /// the fencing contract entirely.
    pub fn initial(actor_id: u64) -> Self {
        Self {
            actor_id,
            epoch: INITIAL_ACTIVATION_EPOCH,
        }
    }
}

/// Highest activation epoch durably accepted for one actor namespace.
#[derive(Debug, Clone, Copy, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
pub struct PersistedActivationFence {
    pub actor_id: u64,
    pub epoch: u64,
}

impl From<ActivationFence> for PersistedActivationFence {
    fn from(fence: ActivationFence) -> Self {
        Self {
            actor_id: fence.actor_id,
            epoch: fence.epoch,
        }
    }
}

/// Decision a persistence backend must enforce atomically with the durable
/// mutation associated with the presented fence.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ActivationFenceDecision {
    /// No fence exists yet: establish the presented epoch and commit the write.
    Initialize { epoch: u64 },
    /// The activation still owns the current epoch: commit normally.
    Current { epoch: u64 },
    /// A newly-authoritative activation presents a higher epoch: atomically
    /// advance the stored fence and commit its first write.
    Advance { previous_epoch: u64, epoch: u64 },
}

impl ActivationFenceDecision {
    /// Fence state that must exist after the corresponding durable write commits.
    pub fn resulting_state(self, actor_id: u64) -> PersistedActivationFence {
        let epoch = match self {
            Self::Initialize { epoch } | Self::Current { epoch } | Self::Advance { epoch, .. } => {
                epoch
            }
        };
        PersistedActivationFence { actor_id, epoch }
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ActivationFenceError {
    ZeroEpoch {
        actor_id: u64,
    },
    CorruptStoredZeroEpoch {
        actor_id: u64,
    },
    NamespaceActorMismatch {
        namespace_actor_id: u64,
        stored_actor_id: u64,
        presented_actor_id: u64,
    },
    StaleActivation {
        actor_id: u64,
        presented_epoch: u64,
        current_epoch: u64,
    },
}

impl std::fmt::Display for ActivationFenceError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::ZeroEpoch { actor_id } => {
                write!(f, "activation fence epoch must be >= 1 for actor {actor_id}")
            }
            Self::CorruptStoredZeroEpoch { actor_id } => write!(
                f,
                "persisted activation fence for actor {actor_id} contains invalid epoch 0"
            ),
            Self::NamespaceActorMismatch {
                namespace_actor_id,
                stored_actor_id,
                presented_actor_id,
            } => write!(
                f,
                "activation fence namespace mismatch: namespace actor {namespace_actor_id}, stored actor {stored_actor_id}, presented actor {presented_actor_id}"
            ),
            Self::StaleActivation {
                actor_id,
                presented_epoch,
                current_epoch,
            } => write!(
                f,
                "stale activation write rejected for actor {actor_id}: presented epoch {presented_epoch}, current epoch {current_epoch}"
            ),
        }
    }
}

impl std::error::Error for ActivationFenceError {}

/// Evaluate a fenced write for a particular durable actor namespace.
///
/// IMPORTANT: this function is a semantic primitive, not a lock. A backend must
/// read the stored fence, evaluate this decision, update/verify the fence, and
/// perform the actor-state mutation in one atomic transaction/batch/CAS boundary.
/// Performing this check and then issuing an unrelated unconditional write is
/// still vulnerable to a stale-writer race.
pub fn evaluate_activation_fence(
    namespace_actor_id: u64,
    stored: Option<PersistedActivationFence>,
    presented: ActivationFence,
) -> Result<ActivationFenceDecision, ActivationFenceError> {
    if presented.epoch == 0 {
        return Err(ActivationFenceError::ZeroEpoch {
            actor_id: presented.actor_id,
        });
    }

    let Some(stored) = stored else {
        if presented.actor_id != namespace_actor_id {
            return Err(ActivationFenceError::NamespaceActorMismatch {
                namespace_actor_id,
                stored_actor_id: namespace_actor_id,
                presented_actor_id: presented.actor_id,
            });
        }
        return Ok(ActivationFenceDecision::Initialize {
            epoch: presented.epoch,
        });
    };

    if stored.epoch == 0 {
        return Err(ActivationFenceError::CorruptStoredZeroEpoch {
            actor_id: stored.actor_id,
        });
    }

    if stored.actor_id != namespace_actor_id || presented.actor_id != namespace_actor_id {
        return Err(ActivationFenceError::NamespaceActorMismatch {
            namespace_actor_id,
            stored_actor_id: stored.actor_id,
            presented_actor_id: presented.actor_id,
        });
    }

    if presented.epoch < stored.epoch {
        return Err(ActivationFenceError::StaleActivation {
            actor_id: namespace_actor_id,
            presented_epoch: presented.epoch,
            current_epoch: stored.epoch,
        });
    }

    if presented.epoch == stored.epoch {
        return Ok(ActivationFenceDecision::Current {
            epoch: stored.epoch,
        });
    }

    Ok(ActivationFenceDecision::Advance {
        previous_epoch: stored.epoch,
        epoch: presented.epoch,
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn initial_epoch_is_one() {
        assert_eq!(ActivationFence::initial(42), ActivationFence { actor_id: 42, epoch: 1 });
    }

    #[test]
    fn zero_presented_epoch_is_invalid() {
        assert_eq!(
            ActivationFence::new(42, 0).unwrap_err(),
            ActivationFenceError::ZeroEpoch { actor_id: 42 }
        );
    }

    #[test]
    fn first_write_initializes_fence() {
        assert_eq!(
            evaluate_activation_fence(42, None, ActivationFence::initial(42)).unwrap(),
            ActivationFenceDecision::Initialize { epoch: 1 }
        );
    }

    #[test]
    fn same_epoch_can_continue_writing() {
        let stored = PersistedActivationFence { actor_id: 42, epoch: 3 };
        let presented = ActivationFence::new(42, 3).unwrap();
        assert_eq!(
            evaluate_activation_fence(42, Some(stored), presented).unwrap(),
            ActivationFenceDecision::Current { epoch: 3 }
        );
    }

    #[test]
    fn higher_epoch_advances_authority() {
        let stored = PersistedActivationFence { actor_id: 42, epoch: 3 };
        let presented = ActivationFence::new(42, 4).unwrap();
        assert_eq!(
            evaluate_activation_fence(42, Some(stored), presented).unwrap(),
            ActivationFenceDecision::Advance {
                previous_epoch: 3,
                epoch: 4,
            }
        );
    }

    #[test]
    fn stale_writer_is_rejected_after_epoch_advance() {
        let stored = PersistedActivationFence { actor_id: 42, epoch: 4 };
        let stale = ActivationFence::new(42, 3).unwrap();
        assert_eq!(
            evaluate_activation_fence(42, Some(stored), stale).unwrap_err(),
            ActivationFenceError::StaleActivation {
                actor_id: 42,
                presented_epoch: 3,
                current_epoch: 4,
            }
        );
    }

    #[test]
    fn actor_namespace_mismatch_fails_closed() {
        let stored = PersistedActivationFence { actor_id: 42, epoch: 2 };
        let presented = ActivationFence::new(99, 2).unwrap();
        assert!(matches!(
            evaluate_activation_fence(42, Some(stored), presented),
            Err(ActivationFenceError::NamespaceActorMismatch { .. })
        ));
    }

    #[test]
    fn corrupt_stored_zero_epoch_fails_closed() {
        let stored = PersistedActivationFence { actor_id: 42, epoch: 0 };
        assert_eq!(
            evaluate_activation_fence(42, Some(stored), ActivationFence::initial(42)).unwrap_err(),
            ActivationFenceError::CorruptStoredZeroEpoch { actor_id: 42 }
        );
    }

    #[test]
    fn decision_resulting_state_matches_committed_authority() {
        let decision = ActivationFenceDecision::Advance {
            previous_epoch: 7,
            epoch: 8,
        };
        assert_eq!(
            decision.resulting_state(42),
            PersistedActivationFence { actor_id: 42, epoch: 8 }
        );
    }
}
