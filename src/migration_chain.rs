//! Deterministic RFC 0008 schema-migration chain planning.
//!
//! Recovery must never guess which migration to apply. This module validates
//! version metadata and returns the exact declaration indices required to move
//! persisted state from an older schema version to the currently loaded entity
//! version. Migration bodies remain compiler-owned; the planner deals only in
//! version edges and declaration indices.

use std::collections::BTreeMap;

/// One migration declaration as seen by the chain planner.
///
/// `declaration_index` points back into the compiler/runtime migration table so
/// the recovery layer can execute already-validated compiled migration code in
/// deterministic order.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct MigrationEdge {
    pub from_version: u32,
    pub to_version: u32,
    pub declaration_index: usize,
}

/// A deterministic adjacent migration step selected for recovery.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct PlannedMigrationStep {
    pub from_version: u32,
    pub to_version: u32,
    pub declaration_index: usize,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum MigrationChainError {
    ZeroPersistedVersion,
    ZeroCurrentVersion,
    ZeroMigrationVersion {
        declaration_index: usize,
        from_version: u32,
        to_version: u32,
    },
    NonAdjacentMigration {
        declaration_index: usize,
        from_version: u32,
        to_version: u32,
    },
    DuplicateFromVersion {
        from_version: u32,
        first_declaration_index: usize,
        second_declaration_index: usize,
    },
    PersistedVersionNewerThanRuntime {
        persisted_version: u32,
        current_version: u32,
    },
    MissingMigrationStep {
        from_version: u32,
        required_to_version: u32,
    },
}

impl std::fmt::Display for MigrationChainError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::ZeroPersistedVersion => {
                write!(f, "persisted entity schema version must be >= 1")
            }
            Self::ZeroCurrentVersion => {
                write!(f, "current entity schema version must be >= 1")
            }
            Self::ZeroMigrationVersion {
                declaration_index,
                from_version,
                to_version,
            } => write!(
                f,
                "migration declaration {declaration_index} uses invalid zero version edge {from_version}->{to_version}"
            ),
            Self::NonAdjacentMigration {
                declaration_index,
                from_version,
                to_version,
            } => write!(
                f,
                "migration declaration {declaration_index} must be adjacent, found {from_version}->{to_version}"
            ),
            Self::DuplicateFromVersion {
                from_version,
                first_declaration_index,
                second_declaration_index,
            } => write!(
                f,
                "ambiguous migration chain from version {from_version}: declarations {first_declaration_index} and {second_declaration_index} both claim that source version"
            ),
            Self::PersistedVersionNewerThanRuntime {
                persisted_version,
                current_version,
            } => write!(
                f,
                "persisted entity schema version {persisted_version} is newer than runtime schema version {current_version}"
            ),
            Self::MissingMigrationStep {
                from_version,
                required_to_version,
            } => write!(
                f,
                "missing migration step {from_version}->{required_to_version}"
            ),
        }
    }
}

impl std::error::Error for MigrationChainError {}

/// Validate the migration registry and plan the exact adjacent chain required
/// to upgrade `persisted_version` to `current_version`.
///
/// RFC 0008 defines migration declarations as `V -> V+1`. The planner validates
/// *all supplied declarations*, not only the subset needed by this particular
/// recovery, so malformed or ambiguous compiled metadata cannot remain latent
/// until an older snapshot happens to exercise it.
pub fn plan_migration_chain<I>(
    persisted_version: u32,
    current_version: u32,
    migrations: I,
) -> Result<Vec<PlannedMigrationStep>, MigrationChainError>
where
    I: IntoIterator<Item = MigrationEdge>,
{
    if persisted_version == 0 {
        return Err(MigrationChainError::ZeroPersistedVersion);
    }
    if current_version == 0 {
        return Err(MigrationChainError::ZeroCurrentVersion);
    }
    if persisted_version > current_version {
        return Err(MigrationChainError::PersistedVersionNewerThanRuntime {
            persisted_version,
            current_version,
        });
    }

    let mut by_from: BTreeMap<u32, MigrationEdge> = BTreeMap::new();
    for edge in migrations {
        if edge.from_version == 0 || edge.to_version == 0 {
            return Err(MigrationChainError::ZeroMigrationVersion {
                declaration_index: edge.declaration_index,
                from_version: edge.from_version,
                to_version: edge.to_version,
            });
        }

        let expected_to = edge.from_version.checked_add(1).ok_or(
            MigrationChainError::NonAdjacentMigration {
                declaration_index: edge.declaration_index,
                from_version: edge.from_version,
                to_version: edge.to_version,
            },
        )?;
        if edge.to_version != expected_to {
            return Err(MigrationChainError::NonAdjacentMigration {
                declaration_index: edge.declaration_index,
                from_version: edge.from_version,
                to_version: edge.to_version,
            });
        }

        if let Some(first) = by_from.insert(edge.from_version, edge) {
            return Err(MigrationChainError::DuplicateFromVersion {
                from_version: edge.from_version,
                first_declaration_index: first.declaration_index,
                second_declaration_index: edge.declaration_index,
            });
        }
    }

    if persisted_version == current_version {
        return Ok(Vec::new());
    }

    let mut plan = Vec::with_capacity((current_version - persisted_version) as usize);
    let mut version = persisted_version;
    while version < current_version {
        let required_to = version + 1;
        let edge = by_from
            .get(&version)
            .ok_or(MigrationChainError::MissingMigrationStep {
                from_version: version,
                required_to_version: required_to,
            })?;

        plan.push(PlannedMigrationStep {
            from_version: edge.from_version,
            to_version: edge.to_version,
            declaration_index: edge.declaration_index,
        });
        version = required_to;
    }

    Ok(plan)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn edge(from: u32, to: u32, declaration_index: usize) -> MigrationEdge {
        MigrationEdge {
            from_version: from,
            to_version: to,
            declaration_index,
        }
    }

    #[test]
    fn same_version_requires_no_migration() {
        let plan = plan_migration_chain(3, 3, [edge(1, 2, 0), edge(2, 3, 1)]).unwrap();
        assert!(plan.is_empty());
    }

    #[test]
    fn plans_single_adjacent_upgrade() {
        let plan = plan_migration_chain(1, 2, [edge(1, 2, 7)]).unwrap();
        assert_eq!(
            plan,
            vec![PlannedMigrationStep {
                from_version: 1,
                to_version: 2,
                declaration_index: 7,
            }]
        );
    }

    #[test]
    fn declaration_order_does_not_change_execution_order() {
        let plan = plan_migration_chain(
            1,
            4,
            [edge(3, 4, 30), edge(1, 2, 10), edge(2, 3, 20)],
        )
        .unwrap();

        assert_eq!(
            plan.iter().map(|step| step.declaration_index).collect::<Vec<_>>(),
            vec![10, 20, 30]
        );
    }

    #[test]
    fn planner_can_start_from_intermediate_persisted_version() {
        let plan = plan_migration_chain(
            2,
            4,
            [edge(1, 2, 10), edge(2, 3, 20), edge(3, 4, 30)],
        )
        .unwrap();

        assert_eq!(
            plan.iter().map(|step| step.from_version).collect::<Vec<_>>(),
            vec![2, 3]
        );
    }

    #[test]
    fn missing_step_fails_closed() {
        let error = plan_migration_chain(1, 4, [edge(1, 2, 0), edge(3, 4, 1)]).unwrap_err();
        assert_eq!(
            error,
            MigrationChainError::MissingMigrationStep {
                from_version: 2,
                required_to_version: 3,
            }
        );
    }

    #[test]
    fn duplicate_source_version_is_ambiguous_even_if_edges_match() {
        let error = plan_migration_chain(1, 2, [edge(1, 2, 4), edge(1, 2, 9)]).unwrap_err();
        assert_eq!(
            error,
            MigrationChainError::DuplicateFromVersion {
                from_version: 1,
                first_declaration_index: 4,
                second_declaration_index: 9,
            }
        );
    }

    #[test]
    fn non_adjacent_forward_jump_is_rejected() {
        let error = plan_migration_chain(1, 3, [edge(1, 3, 0)]).unwrap_err();
        assert_eq!(
            error,
            MigrationChainError::NonAdjacentMigration {
                declaration_index: 0,
                from_version: 1,
                to_version: 3,
            }
        );
    }

    #[test]
    fn downgrade_edge_is_rejected_as_non_adjacent() {
        let error = plan_migration_chain(1, 2, [edge(2, 1, 0), edge(1, 2, 1)]).unwrap_err();
        assert_eq!(
            error,
            MigrationChainError::NonAdjacentMigration {
                declaration_index: 0,
                from_version: 2,
                to_version: 1,
            }
        );
    }

    #[test]
    fn persisted_version_newer_than_runtime_fails_closed() {
        let error = plan_migration_chain(4, 3, []).unwrap_err();
        assert_eq!(
            error,
            MigrationChainError::PersistedVersionNewerThanRuntime {
                persisted_version: 4,
                current_version: 3,
            }
        );
    }

    #[test]
    fn zero_versions_are_rejected() {
        assert_eq!(
            plan_migration_chain(0, 1, []).unwrap_err(),
            MigrationChainError::ZeroPersistedVersion
        );
        assert_eq!(
            plan_migration_chain(1, 0, []).unwrap_err(),
            MigrationChainError::ZeroCurrentVersion
        );
        assert!(matches!(
            plan_migration_chain(1, 2, [edge(0, 1, 0)]),
            Err(MigrationChainError::ZeroMigrationVersion { .. })
        ));
    }

    #[test]
    fn malformed_unused_declaration_still_fails_registry_validation() {
        let error = plan_migration_chain(3, 3, [edge(7, 9, 22)]).unwrap_err();
        assert!(matches!(
            error,
            MigrationChainError::NonAdjacentMigration {
                declaration_index: 22,
                ..
            }
        ));
    }
}
