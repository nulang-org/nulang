//! Persisted entity schema identity and recovery compatibility checks.
//!
//! RFC 0008 schema versions choose *which migration chain* is required. A stable
//! entity name plus same-version type hash additionally prevents silent schema
//! changes that forget to bump the declared version.

pub const SCHEMA_IDENTITY_RECORD_VERSION: u8 = 1;

#[derive(Debug, Clone, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
pub struct EntitySchemaIdentity {
    pub record_version: u8,
    pub entity_name: String,
    pub schema_version: u32,
    /// Compiler-produced stable schema/type digest for this exact declared
    /// version. Different legitimate schema versions are expected to differ.
    pub type_hash: Option<[u8; 32]>,
}

impl EntitySchemaIdentity {
    pub fn new(
        entity_name: impl Into<String>,
        schema_version: u32,
        type_hash: Option<[u8; 32]>,
    ) -> Self {
        Self {
            record_version: SCHEMA_IDENTITY_RECORD_VERSION,
            entity_name: entity_name.into(),
            schema_version,
            type_hash,
        }
    }
}

/// Policy for historical metadata created before stable type hashes are present.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum MissingTypeHashPolicy {
    /// Production-safe default: do not claim same-version compatibility without
    /// a digest proving that the schema did not change silently.
    Reject,
    /// Explicit compatibility/import path for known legacy state. This should be
    /// opt-in and observable; it is not the default recovery behavior.
    AllowLegacy,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum SchemaRecoveryDecision {
    /// Persisted state exactly matches the current declared version and hash.
    Current,
    /// Same declared version but one/both historical hashes are absent under an
    /// explicit legacy policy.
    CurrentLegacyUnverified,
    /// Persisted state is older and must run the deterministic RFC 0008 chain.
    Migrate {
        from_version: u32,
        to_version: u32,
    },
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum SchemaIdentityError {
    UnsupportedRecordVersion {
        found: u8,
        supported: u8,
    },
    ZeroPersistedSchemaVersion,
    ZeroCurrentSchemaVersion,
    EntityNameMismatch {
        persisted: String,
        current: String,
    },
    PersistedVersionNewerThanRuntime {
        persisted_version: u32,
        current_version: u32,
    },
    SameVersionTypeHashMismatch {
        entity_name: String,
        schema_version: u32,
        persisted_hash: [u8; 32],
        current_hash: [u8; 32],
    },
    MissingSameVersionTypeHash {
        entity_name: String,
        schema_version: u32,
        persisted_missing: bool,
        current_missing: bool,
    },
}

impl std::fmt::Display for SchemaIdentityError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::UnsupportedRecordVersion { found, supported } => write!(
                f,
                "unsupported schema identity record version {found}; runtime supports {supported}"
            ),
            Self::ZeroPersistedSchemaVersion => {
                write!(f, "persisted entity schema version must be >= 1")
            }
            Self::ZeroCurrentSchemaVersion => {
                write!(f, "current entity schema version must be >= 1")
            }
            Self::EntityNameMismatch { persisted, current } => write!(
                f,
                "persisted entity identity mismatch: stored {persisted:?}, current {current:?}"
            ),
            Self::PersistedVersionNewerThanRuntime {
                persisted_version,
                current_version,
            } => write!(
                f,
                "persisted entity schema version {persisted_version} is newer than runtime schema version {current_version}"
            ),
            Self::SameVersionTypeHashMismatch {
                entity_name,
                schema_version,
                ..
            } => write!(
                f,
                "entity {entity_name:?} schema version {schema_version} has a different type hash than persisted state; bump the schema version and provide a migration"
            ),
            Self::MissingSameVersionTypeHash {
                entity_name,
                schema_version,
                persisted_missing,
                current_missing,
            } => write!(
                f,
                "cannot verify same-version schema identity for {entity_name:?} v{schema_version}: persisted hash missing={persisted_missing}, current hash missing={current_missing}"
            ),
        }
    }
}

impl std::error::Error for SchemaIdentityError {}

/// Decide whether persisted state is directly compatible, requires migration,
/// or must fail closed.
///
/// Hashes are compared only when versions are equal. When persisted state is
/// older, a differing hash is expected and the migration chain is responsible
/// for transforming it into the current schema.
pub fn evaluate_schema_recovery(
    persisted: &EntitySchemaIdentity,
    current: &EntitySchemaIdentity,
    missing_hash_policy: MissingTypeHashPolicy,
) -> Result<SchemaRecoveryDecision, SchemaIdentityError> {
    if persisted.record_version != SCHEMA_IDENTITY_RECORD_VERSION {
        return Err(SchemaIdentityError::UnsupportedRecordVersion {
            found: persisted.record_version,
            supported: SCHEMA_IDENTITY_RECORD_VERSION,
        });
    }
    if current.record_version != SCHEMA_IDENTITY_RECORD_VERSION {
        return Err(SchemaIdentityError::UnsupportedRecordVersion {
            found: current.record_version,
            supported: SCHEMA_IDENTITY_RECORD_VERSION,
        });
    }
    if persisted.schema_version == 0 {
        return Err(SchemaIdentityError::ZeroPersistedSchemaVersion);
    }
    if current.schema_version == 0 {
        return Err(SchemaIdentityError::ZeroCurrentSchemaVersion);
    }
    if persisted.entity_name != current.entity_name {
        return Err(SchemaIdentityError::EntityNameMismatch {
            persisted: persisted.entity_name.clone(),
            current: current.entity_name.clone(),
        });
    }
    if persisted.schema_version > current.schema_version {
        return Err(SchemaIdentityError::PersistedVersionNewerThanRuntime {
            persisted_version: persisted.schema_version,
            current_version: current.schema_version,
        });
    }
    if persisted.schema_version < current.schema_version {
        return Ok(SchemaRecoveryDecision::Migrate {
            from_version: persisted.schema_version,
            to_version: current.schema_version,
        });
    }

    match (persisted.type_hash, current.type_hash) {
        (Some(persisted_hash), Some(current_hash)) if persisted_hash == current_hash => {
            Ok(SchemaRecoveryDecision::Current)
        }
        (Some(persisted_hash), Some(current_hash)) => {
            Err(SchemaIdentityError::SameVersionTypeHashMismatch {
                entity_name: current.entity_name.clone(),
                schema_version: current.schema_version,
                persisted_hash,
                current_hash,
            })
        }
        (persisted_hash, current_hash) => {
            if missing_hash_policy == MissingTypeHashPolicy::AllowLegacy {
                Ok(SchemaRecoveryDecision::CurrentLegacyUnverified)
            } else {
                Err(SchemaIdentityError::MissingSameVersionTypeHash {
                    entity_name: current.entity_name.clone(),
                    schema_version: current.schema_version,
                    persisted_missing: persisted_hash.is_none(),
                    current_missing: current_hash.is_none(),
                })
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn hash(byte: u8) -> [u8; 32] {
        [byte; 32]
    }

    #[test]
    fn matching_same_version_hash_is_current() {
        let persisted = EntitySchemaIdentity::new("Account", 2, Some(hash(7)));
        let current = EntitySchemaIdentity::new("Account", 2, Some(hash(7)));
        assert_eq!(
            evaluate_schema_recovery(&persisted, &current, MissingTypeHashPolicy::Reject).unwrap(),
            SchemaRecoveryDecision::Current
        );
    }

    #[test]
    fn same_version_hash_mismatch_fails_closed() {
        let persisted = EntitySchemaIdentity::new("Account", 2, Some(hash(7)));
        let current = EntitySchemaIdentity::new("Account", 2, Some(hash(8)));
        assert!(matches!(
            evaluate_schema_recovery(&persisted, &current, MissingTypeHashPolicy::Reject),
            Err(SchemaIdentityError::SameVersionTypeHashMismatch { .. })
        ));
    }

    #[test]
    fn older_version_requires_migration_without_comparing_hashes() {
        let persisted = EntitySchemaIdentity::new("Account", 1, Some(hash(1)));
        let current = EntitySchemaIdentity::new("Account", 3, Some(hash(99)));
        assert_eq!(
            evaluate_schema_recovery(&persisted, &current, MissingTypeHashPolicy::Reject).unwrap(),
            SchemaRecoveryDecision::Migrate {
                from_version: 1,
                to_version: 3,
            }
        );
    }

    #[test]
    fn newer_persisted_version_rejects_runtime_downgrade() {
        let persisted = EntitySchemaIdentity::new("Account", 4, Some(hash(4)));
        let current = EntitySchemaIdentity::new("Account", 3, Some(hash(3)));
        assert_eq!(
            evaluate_schema_recovery(&persisted, &current, MissingTypeHashPolicy::Reject)
                .unwrap_err(),
            SchemaIdentityError::PersistedVersionNewerThanRuntime {
                persisted_version: 4,
                current_version: 3,
            }
        );
    }

    #[test]
    fn entity_name_mismatch_fails_before_state_is_reinterpreted() {
        let persisted = EntitySchemaIdentity::new("Account", 1, Some(hash(1)));
        let current = EntitySchemaIdentity::new("Invoice", 2, Some(hash(2)));
        assert!(matches!(
            evaluate_schema_recovery(&persisted, &current, MissingTypeHashPolicy::Reject),
            Err(SchemaIdentityError::EntityNameMismatch { .. })
        ));
    }

    #[test]
    fn missing_hash_fails_by_default_for_same_version() {
        let persisted = EntitySchemaIdentity::new("Account", 2, None);
        let current = EntitySchemaIdentity::new("Account", 2, Some(hash(2)));
        assert!(matches!(
            evaluate_schema_recovery(&persisted, &current, MissingTypeHashPolicy::Reject),
            Err(SchemaIdentityError::MissingSameVersionTypeHash {
                persisted_missing: true,
                current_missing: false,
                ..
            })
        ));
    }

    #[test]
    fn missing_hash_requires_explicit_legacy_policy() {
        let persisted = EntitySchemaIdentity::new("Account", 1, None);
        let current = EntitySchemaIdentity::new("Account", 1, None);
        assert_eq!(
            evaluate_schema_recovery(
                &persisted,
                &current,
                MissingTypeHashPolicy::AllowLegacy,
            )
            .unwrap(),
            SchemaRecoveryDecision::CurrentLegacyUnverified
        );
    }

    #[test]
    fn zero_versions_are_invalid() {
        let persisted = EntitySchemaIdentity::new("Account", 0, Some(hash(1)));
        let current = EntitySchemaIdentity::new("Account", 1, Some(hash(1)));
        assert_eq!(
            evaluate_schema_recovery(&persisted, &current, MissingTypeHashPolicy::Reject)
                .unwrap_err(),
            SchemaIdentityError::ZeroPersistedSchemaVersion
        );
    }

    #[test]
    fn unsupported_record_version_fails_closed() {
        let mut persisted = EntitySchemaIdentity::new("Account", 1, Some(hash(1)));
        persisted.record_version = SCHEMA_IDENTITY_RECORD_VERSION + 1;
        let current = EntitySchemaIdentity::new("Account", 1, Some(hash(1)));
        assert!(matches!(
            evaluate_schema_recovery(&persisted, &current, MissingTypeHashPolicy::Reject),
            Err(SchemaIdentityError::UnsupportedRecordVersion { .. })
        ));
    }
}
