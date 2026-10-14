//! Compiler-owned actor state schema upgrade preflight.
//!
//! This is intentionally **evidence only**. A declared migration chain does
//! not prove the bodies are pure, preserve durable histories, or can recover
//! an activation. The runtime must still enforce semantic/artifact identity,
//! migration validation, and atomic persistence at the actual commit boundary.

use std::collections::BTreeMap;

use crate::semantic_schema::{
    canonical_actor_state_schema_bytes, ActorStateField, ActorStateSchema,
};

/// Human-actionable changes between two compiler-derived actor schemas.
///
/// These are *diagnostics*, not permission to deserialize or resume a
/// persisted activation with the new schema.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ActorSchemaDifference {
    ActorChanged { before: String, after: String },
    VersionRegressed { before: u32, after: u32 },
    ChangedWithoutVersionBump { version: u32 },
    FieldAdded { field: String },
    FieldRemoved { field: String },
    FieldTypeChanged { field: String },
}

/// Whether a forward, contiguous migration chain is *declared* in the new
/// compiler-derived schema. Complete is not a semantic or runtime approval.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum DeclaredMigrationPath {
    /// The canonical state schema and migration metadata are identical.
    NotRequired,
    /// There is exactly one declared step for each forward version transition.
    Complete,
    /// Actor identity changed, the version regressed, or unversioned drift
    /// exists. No safe forward migration path can be established from metadata.
    NotApplicable,
    MissingStep {
        from_version: u32,
        to_version: u32,
    },
    AmbiguousStep {
        from_version: u32,
    },
    InvalidStep {
        from_version: u32,
        to_version: u32,
        expected_to_version: u32,
    },
}

/// A deterministic, side-effect-free report over two *typed* actor schemas.
///
/// Do not use this report as the final authorization decision for recovery.
/// It cannot prove migration purity, provider effect safety, protocol
/// compatibility, or retained historical artifact availability.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ActorSchemaUpgradeReport {
    pub same_canonical_schema: bool,
    pub differences: Vec<ActorSchemaDifference>,
    pub migration_path: DeclaredMigrationPath,
}

/// Compare the old and new compiler-derived actor state schemas.
///
/// Field changes are emitted deterministically: removals/type changes in old
/// field-name order, followed by additions in new field-name order. The exact
/// comparison uses Nulang's existing canonical schema encoding, so source
/// declaration order never creates false changes.
pub fn analyze_actor_schema_upgrade(
    before: &ActorStateSchema,
    after: &ActorStateSchema,
) -> ActorSchemaUpgradeReport {
    let same_canonical_schema =
        canonical_actor_state_schema_bytes(std::slice::from_ref(before))
            == canonical_actor_state_schema_bytes(std::slice::from_ref(after));

    if same_canonical_schema {
        return ActorSchemaUpgradeReport {
            same_canonical_schema: true,
            differences: vec![],
            migration_path: DeclaredMigrationPath::NotRequired,
        };
    }

    let mut differences = Vec::new();
    if before.actor_name != after.actor_name {
        differences.push(ActorSchemaDifference::ActorChanged {
            before: before.actor_name.clone(),
            after: after.actor_name.clone(),
        });
    }
    if after.version < before.version {
        differences.push(ActorSchemaDifference::VersionRegressed {
            before: before.version,
            after: after.version,
        });
    }

    let old_fields: BTreeMap<&str, &ActorStateField> =
        before.fields.iter().map(|field| (field.name.as_str(), field)).collect();
    let new_fields: BTreeMap<&str, &ActorStateField> =
        after.fields.iter().map(|field| (field.name.as_str(), field)).collect();

    for (name, previous) in &old_fields {
        match new_fields.get(name) {
            None => differences.push(ActorSchemaDifference::FieldRemoved {
                field: (*name).to_owned(),
            }),
            Some(current) if previous.ty != current.ty => {
                differences.push(ActorSchemaDifference::FieldTypeChanged {
                    field: (*name).to_owned(),
                });
            }
            Some(_) => {}
        }
    }
    for name in new_fields.keys() {
        if !old_fields.contains_key(name) {
            differences.push(ActorSchemaDifference::FieldAdded {
                field: (*name).to_owned(),
            });
        }
    }

    if before.version == after.version {
        differences.push(ActorSchemaDifference::ChangedWithoutVersionBump {
            version: before.version,
        });
    }

    let migration_path = if before.actor_name != after.actor_name
        || after.version <= before.version
    {
        DeclaredMigrationPath::NotApplicable
    } else {
        inspect_declared_migration_path(before.version, after.version, after)
    };

    ActorSchemaUpgradeReport {
        same_canonical_schema,
        differences,
        migration_path,
    }
}

fn inspect_declared_migration_path(
    from_version: u32,
    to_version: u32,
    current: &ActorStateSchema,
) -> DeclaredMigrationPath {
    let mut version = from_version;
    while version < to_version {
        let expected_to_version = version + 1;
        let mut steps = current
            .migrations
            .iter()
            .filter(|step| step.from_version == version);
        let Some(step) = steps.next() else {
            return DeclaredMigrationPath::MissingStep {
                from_version: version,
                to_version: expected_to_version,
            };
        };
        if steps.next().is_some() {
            return DeclaredMigrationPath::AmbiguousStep {
                from_version: version,
            };
        }
        if step.to_version != expected_to_version {
            return DeclaredMigrationPath::InvalidStep {
                from_version: version,
                to_version: step.to_version,
                expected_to_version,
            };
        }
        version = expected_to_version;
    }
    DeclaredMigrationPath::Complete
}
