//! Recovery policy for durable grain namespaces.
//!
//! A missing logical-identity record is safe only when the compact-id namespace
//! is otherwise empty. Existing durable state without identity metadata is
//! legacy/ambiguous and must not be assigned to whichever `GrainId` happens to
//! request the 48-bit namespace after restart.

use crate::grain_identity::{PersistedGrainIdentity, PersistedGrainIdentityError};
use crate::runtime::GrainId;
use crate::types::{NuError, Span};

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum GrainIdentityRecoveryDecision {
    /// Existing metadata was present and matched the requested logical grain.
    VerifiedExistingIdentity,
    /// The durable namespace is empty and may be claimed for the requested grain
    /// before any snapshot/journal/event state is written.
    ClaimEmptyNamespace,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum GrainIdentityRecoveryError {
    InvalidIdentity(PersistedGrainIdentityError),
    /// Durable state predates authoritative grain identity metadata. Adopting it
    /// automatically would make a compact-hash collision indistinguishable from
    /// legitimate ownership after restart.
    LegacyStateWithoutIdentity {
        actor_id: u64,
        requested: GrainId,
    },
}

impl std::fmt::Display for GrainIdentityRecoveryError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::InvalidIdentity(error) => write!(f, "{error}"),
            Self::LegacyStateWithoutIdentity {
                actor_id,
                requested,
            } => write!(
                f,
                "durable grain namespace {} contains state but no logical identity metadata; refusing to assign it to {:?} without explicit migration/import",
                actor_id, requested
            ),
        }
    }
}

impl std::error::Error for GrainIdentityRecoveryError {}

impl From<PersistedGrainIdentityError> for GrainIdentityRecoveryError {
    fn from(error: PersistedGrainIdentityError) -> Self {
        Self::InvalidIdentity(error)
    }
}

impl From<GrainIdentityRecoveryError> for NuError {
    fn from(error: GrainIdentityRecoveryError) -> Self {
        NuError::RuntimeError {
            msg: error.to_string(),
            span: Span::new(0, 0),
        }
    }
}

/// Decide whether a compact-id durable namespace can be used for a requested
/// logical grain.
///
/// `namespace_has_state` must conservatively mean that any durable artifact
/// already exists under the namespace (snapshot, message/event/workflow journal,
/// timer/signal state, or backend-equivalent metadata). False negatives here
/// would permit accidental adoption of legacy state, so backends should prefer
/// "occupied" when uncertain.
pub(crate) fn evaluate_grain_identity_recovery(
    persisted_identity: Option<&PersistedGrainIdentity>,
    requested: &GrainId,
    namespace_actor_id: u64,
    namespace_has_state: bool,
) -> Result<GrainIdentityRecoveryDecision, GrainIdentityRecoveryError> {
    if let Some(identity) = persisted_identity {
        identity.validate_for_request(requested, namespace_actor_id)?;
        return Ok(GrainIdentityRecoveryDecision::VerifiedExistingIdentity);
    }

    if namespace_has_state {
        return Err(GrainIdentityRecoveryError::LegacyStateWithoutIdentity {
            actor_id: namespace_actor_id,
            requested: requested.clone(),
        });
    }

    Ok(GrainIdentityRecoveryDecision::ClaimEmptyNamespace)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::grain_identity::PersistedGrainIdentity;
    use crate::runtime::grain_actor_id;

    #[test]
    fn matching_metadata_verifies_existing_namespace() {
        let grain = GrainId::new("User", "42");
        let actor_id = grain_actor_id(&grain);
        let identity = PersistedGrainIdentity::current(&grain);

        assert_eq!(
            evaluate_grain_identity_recovery(Some(&identity), &grain, actor_id, true).unwrap(),
            GrainIdentityRecoveryDecision::VerifiedExistingIdentity
        );
    }

    #[test]
    fn empty_namespace_without_metadata_must_be_claimed_first() {
        let grain = GrainId::new("User", "42");
        let actor_id = grain_actor_id(&grain);

        assert_eq!(
            evaluate_grain_identity_recovery(None, &grain, actor_id, false).unwrap(),
            GrainIdentityRecoveryDecision::ClaimEmptyNamespace
        );
    }

    #[test]
    fn legacy_state_without_identity_fails_closed() {
        let grain = GrainId::new("User", "42");
        let actor_id = grain_actor_id(&grain);

        let error = evaluate_grain_identity_recovery(None, &grain, actor_id, true).unwrap_err();
        assert_eq!(
            error,
            GrainIdentityRecoveryError::LegacyStateWithoutIdentity {
                actor_id,
                requested: grain,
            }
        );
    }

    #[test]
    fn mismatched_existing_identity_fails_closed() {
        let stored = GrainId::new("User", "42");
        let requested = GrainId::new("Order", "42");
        let identity = PersistedGrainIdentity::current(&stored);

        // Directly exercise the logical-identity policy using the stored
        // namespace. Real compact collisions reach the same branch because the
        // requested projection equals the occupied namespace.
        let error = evaluate_grain_identity_recovery(
            Some(&identity),
            &requested,
            identity.actor_id,
            true,
        )
        .unwrap_err();
        assert!(matches!(
            error,
            GrainIdentityRecoveryError::InvalidIdentity(_)
        ));
    }

    #[test]
    fn recovery_error_converts_to_runtime_error() {
        let error = GrainIdentityRecoveryError::LegacyStateWithoutIdentity {
            actor_id: 7,
            requested: GrainId::new("User", "42"),
        };
        let runtime_error: NuError = error.into();
        assert!(runtime_error
            .to_string()
            .contains("contains state but no logical identity metadata"));
    }
}
