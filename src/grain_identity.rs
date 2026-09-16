//! Durable logical identity metadata for virtual actors (grains).
//!
//! The runtime currently projects [`GrainId`] into a 48-bit actor id so grains
//! can travel through `Value::actor_ref`. That compact id is routing/storage ABI,
//! not authoritative identity. This module defines the versioned record that
//! persistence backends should store alongside grain state and the fail-closed
//! validation used before a compact-id namespace is trusted after restart.

use crate::runtime::{grain_actor_id, GrainId, GRAIN_ACTOR_ID_ALGORITHM_VERSION};
use crate::types::{NuError, Span};

/// Version of the durable grain-identity record schema.
pub const GRAIN_IDENTITY_RECORD_VERSION: u8 = 1;

/// Authoritative logical identity metadata persisted beside durable grain state.
///
/// `actor_id` is intentionally retained because existing v1 persistence is
/// namespaced by the 48-bit projection. It is cross-checked on recovery rather
/// than treated as the identity itself.
#[derive(Debug, Clone, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
pub struct PersistedGrainIdentity {
    pub record_version: u8,
    pub projection_algorithm_version: u8,
    pub actor_id: u64,
    pub grain_type: String,
    pub key: String,
}

impl PersistedGrainIdentity {
    /// Build the current v1 record for a logical grain.
    pub fn current(grain_id: &GrainId) -> Self {
        Self {
            record_version: GRAIN_IDENTITY_RECORD_VERSION,
            projection_algorithm_version: GRAIN_ACTOR_ID_ALGORITHM_VERSION,
            actor_id: grain_actor_id(grain_id),
            grain_type: grain_id.grain_type.clone(),
            key: grain_id.key.clone(),
        }
    }

    /// Reconstruct the structured logical identity stored in this record.
    pub fn grain_id(&self) -> GrainId {
        GrainId::new(self.grain_type.clone(), self.key.clone())
    }

    /// Validate the record itself against the compact storage namespace from
    /// which it was loaded.
    ///
    /// This must run before durable state under `namespace_actor_id` is decoded
    /// into an actor. A malformed/stale record is corruption, not permission to
    /// reinterpret that namespace as a different grain.
    pub fn validate_namespace(
        &self,
        namespace_actor_id: u64,
    ) -> Result<(), PersistedGrainIdentityError> {
        if self.record_version != GRAIN_IDENTITY_RECORD_VERSION {
            return Err(PersistedGrainIdentityError::UnsupportedRecordVersion {
                found: self.record_version,
                supported: GRAIN_IDENTITY_RECORD_VERSION,
            });
        }

        if self.projection_algorithm_version != GRAIN_ACTOR_ID_ALGORITHM_VERSION {
            return Err(
                PersistedGrainIdentityError::UnsupportedProjectionAlgorithmVersion {
                    found: self.projection_algorithm_version,
                    supported: GRAIN_ACTOR_ID_ALGORITHM_VERSION,
                },
            );
        }

        if self.actor_id != namespace_actor_id {
            return Err(PersistedGrainIdentityError::NamespaceActorIdMismatch {
                namespace_actor_id,
                stored_actor_id: self.actor_id,
            });
        }

        let stored_grain = self.grain_id();
        let recomputed_actor_id = grain_actor_id(&stored_grain);
        if recomputed_actor_id != self.actor_id {
            return Err(PersistedGrainIdentityError::StoredProjectionMismatch {
                grain_id: stored_grain,
                stored_actor_id: self.actor_id,
                recomputed_actor_id,
            });
        }

        Ok(())
    }

    /// Validate that this durable namespace belongs to the requested logical
    /// grain rather than another grain with the same compact projection.
    pub fn validate_requested_identity(
        &self,
        requested: &GrainId,
    ) -> Result<(), PersistedGrainIdentityError> {
        let stored = self.grain_id();
        if &stored != requested {
            return Err(PersistedGrainIdentityError::LogicalIdentityMismatch {
                actor_id: self.actor_id,
                stored,
                requested: requested.clone(),
            });
        }
        Ok(())
    }

    /// Full recovery-boundary validation for a requested grain.
    ///
    /// Callers should compute/pass the compact namespace they are about to
    /// access. This method proves (1) the requested grain projects to that
    /// namespace, (2) the stored metadata is internally valid, and (3) the
    /// stored logical identity exactly matches the request.
    pub fn validate_for_request(
        &self,
        requested: &GrainId,
        namespace_actor_id: u64,
    ) -> Result<(), PersistedGrainIdentityError> {
        let requested_actor_id = grain_actor_id(requested);
        if requested_actor_id != namespace_actor_id {
            return Err(PersistedGrainIdentityError::RequestedProjectionMismatch {
                grain_id: requested.clone(),
                namespace_actor_id,
                recomputed_actor_id: requested_actor_id,
            });
        }

        self.validate_namespace(namespace_actor_id)?;
        self.validate_requested_identity(requested)
    }
}

/// Fail-closed durable grain identity validation errors.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum PersistedGrainIdentityError {
    UnsupportedRecordVersion {
        found: u8,
        supported: u8,
    },
    UnsupportedProjectionAlgorithmVersion {
        found: u8,
        supported: u8,
    },
    NamespaceActorIdMismatch {
        namespace_actor_id: u64,
        stored_actor_id: u64,
    },
    StoredProjectionMismatch {
        grain_id: GrainId,
        stored_actor_id: u64,
        recomputed_actor_id: u64,
    },
    RequestedProjectionMismatch {
        grain_id: GrainId,
        namespace_actor_id: u64,
        recomputed_actor_id: u64,
    },
    LogicalIdentityMismatch {
        actor_id: u64,
        stored: GrainId,
        requested: GrainId,
    },
}

impl std::fmt::Display for PersistedGrainIdentityError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::UnsupportedRecordVersion { found, supported } => write!(
                f,
                "unsupported persisted grain identity record version {found}; runtime supports {supported}"
            ),
            Self::UnsupportedProjectionAlgorithmVersion { found, supported } => write!(
                f,
                "unsupported persisted grain projection version {found}; runtime supports {supported}"
            ),
            Self::NamespaceActorIdMismatch {
                namespace_actor_id,
                stored_actor_id,
            } => write!(
                f,
                "persisted grain identity namespace mismatch: storage actor {namespace_actor_id}, metadata actor {stored_actor_id}"
            ),
            Self::StoredProjectionMismatch {
                grain_id,
                stored_actor_id,
                recomputed_actor_id,
            } => write!(
                f,
                "persisted grain identity projection mismatch for {:?}: stored actor {}, recomputed actor {}",
                grain_id, stored_actor_id, recomputed_actor_id
            ),
            Self::RequestedProjectionMismatch {
                grain_id,
                namespace_actor_id,
                recomputed_actor_id,
            } => write!(
                f,
                "requested grain {:?} does not project to storage actor {} (recomputed {})",
                grain_id, namespace_actor_id, recomputed_actor_id
            ),
            Self::LogicalIdentityMismatch {
                actor_id,
                stored,
                requested,
            } => write!(
                f,
                "persisted grain identity collision at actor {}: stored {:?}, requested {:?}",
                actor_id, stored, requested
            ),
        }
    }
}

impl std::error::Error for PersistedGrainIdentityError {}

impl From<PersistedGrainIdentityError> for NuError {
    fn from(error: PersistedGrainIdentityError) -> Self {
        NuError::RuntimeError {
            msg: error.to_string(),
            span: Span::new(0, 0),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn current_record_freezes_projection_metadata() {
        let grain = GrainId::new("User", "42");
        let record = PersistedGrainIdentity::current(&grain);

        assert_eq!(record.record_version, GRAIN_IDENTITY_RECORD_VERSION);
        assert_eq!(
            record.projection_algorithm_version,
            GRAIN_ACTOR_ID_ALGORITHM_VERSION
        );
        assert_eq!(record.actor_id, grain_actor_id(&grain));
        assert_eq!(record.grain_id(), grain);
    }

    #[test]
    fn identity_record_round_trips_through_json() {
        let grain = GrainId::new("User", "é/🔑");
        let record = PersistedGrainIdentity::current(&grain);
        let json = serde_json::to_string(&record).unwrap();
        let decoded: PersistedGrainIdentity = serde_json::from_str(&json).unwrap();

        assert_eq!(decoded, record);
        decoded
            .validate_for_request(&grain, grain_actor_id(&grain))
            .unwrap();
    }

    #[test]
    fn valid_record_matches_requested_grain_and_namespace() {
        let grain = GrainId::new("Counter", "alpha");
        let actor_id = grain_actor_id(&grain);
        let record = PersistedGrainIdentity::current(&grain);

        assert_eq!(record.validate_for_request(&grain, actor_id), Ok(()));
    }

    #[test]
    fn unsupported_record_version_fails_closed() {
        let grain = GrainId::new("User", "42");
        let mut record = PersistedGrainIdentity::current(&grain);
        record.record_version = GRAIN_IDENTITY_RECORD_VERSION + 1;

        assert!(matches!(
            record.validate_namespace(grain_actor_id(&grain)),
            Err(PersistedGrainIdentityError::UnsupportedRecordVersion { .. })
        ));
    }

    #[test]
    fn unsupported_projection_version_fails_closed() {
        let grain = GrainId::new("User", "42");
        let mut record = PersistedGrainIdentity::current(&grain);
        record.projection_algorithm_version = GRAIN_ACTOR_ID_ALGORITHM_VERSION + 1;

        assert!(matches!(
            record.validate_namespace(grain_actor_id(&grain)),
            Err(PersistedGrainIdentityError::UnsupportedProjectionAlgorithmVersion { .. })
        ));
    }

    #[test]
    fn storage_namespace_mismatch_fails_closed() {
        let grain = GrainId::new("User", "42");
        let record = PersistedGrainIdentity::current(&grain);

        assert!(matches!(
            record.validate_namespace(record.actor_id + 1),
            Err(PersistedGrainIdentityError::NamespaceActorIdMismatch { .. })
        ));
    }

    #[test]
    fn corrupted_stored_projection_fails_closed() {
        let grain = GrainId::new("User", "42");
        let mut record = PersistedGrainIdentity::current(&grain);
        record.actor_id = record.actor_id.wrapping_add(1) & ((1_u64 << 48) - 1);

        assert!(matches!(
            record.validate_namespace(record.actor_id),
            Err(PersistedGrainIdentityError::StoredProjectionMismatch { .. })
        ));
    }

    #[test]
    fn different_logical_identity_is_never_accepted() {
        let stored = GrainId::new("User", "42");
        let requested = GrainId::new("Order", "42");
        let record = PersistedGrainIdentity::current(&stored);

        let error = record
            .validate_requested_identity(&requested)
            .unwrap_err();
        assert_eq!(
            error,
            PersistedGrainIdentityError::LogicalIdentityMismatch {
                actor_id: record.actor_id,
                stored,
                requested,
            }
        );
    }

    #[test]
    fn caller_cannot_validate_request_against_wrong_namespace() {
        let grain = GrainId::new("User", "42");
        let record = PersistedGrainIdentity::current(&grain);
        let wrong_namespace = record.actor_id.wrapping_add(1) & ((1_u64 << 48) - 1);

        assert!(matches!(
            record.validate_for_request(&grain, wrong_namespace),
            Err(PersistedGrainIdentityError::RequestedProjectionMismatch { .. })
        ));
    }

    #[test]
    fn persisted_identity_error_converts_to_runtime_error() {
        let error = PersistedGrainIdentityError::LogicalIdentityMismatch {
            actor_id: 7,
            stored: GrainId::new("User", "42"),
            requested: GrainId::new("Order", "42"),
        };
        let runtime_error: NuError = error.into();

        assert!(runtime_error
            .to_string()
            .contains("persisted grain identity collision at actor 7"));
    }
}
