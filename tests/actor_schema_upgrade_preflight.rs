//! Evidence-only schema upgrade preflight tests. No runtime authorization.

use nulang::schema_upgrade::{
    analyze_actor_schema_upgrade, ActorSchemaDifference, DeclaredMigrationPath,
};
use nulang::semantic_schema::{ActorMigrationSchema, ActorStateField, ActorStateSchema};
use nulang::types::{PrimitiveType, Type};

fn schema(name: &str, version: u32, field_type: PrimitiveType) -> ActorStateSchema {
    ActorStateSchema {
        actor_name: name.to_owned(),
        version,
        migrations: vec![],
        fields: vec![ActorStateField {
            name: "value".to_owned(),
            ty: Type::Primitive(field_type),
        }],
    }
}

fn step(from_version: u32, to_version: u32) -> ActorMigrationSchema {
    ActorMigrationSchema {
        from_version,
        to_version,
        has_state_migration: true,
        event_handlers: vec![],
    }
}

#[test]
fn identical_schema_and_reordered_fields_need_no_migration() {
    let mut before = schema("app::Counter", 1, PrimitiveType::Int);
    before.fields.push(ActorStateField {
        name: "label".into(),
        ty: Type::Primitive(PrimitiveType::String),
    });
    let mut after = before.clone();
    after.fields.reverse();

    let report = analyze_actor_schema_upgrade(&before, &after);
    assert!(report.same_canonical_schema);
    assert!(report.differences.is_empty());
    assert_eq!(report.migration_path, DeclaredMigrationPath::NotRequired);
}

#[test]
fn changing_field_type_without_version_bump_is_reported() {
    let before = schema("app::Counter", 1, PrimitiveType::Int);
    let after = schema("app::Counter", 1, PrimitiveType::String);

    let report = analyze_actor_schema_upgrade(&before, &after);
    assert!(!report.same_canonical_schema);
    assert!(report.differences.contains(&ActorSchemaDifference::FieldTypeChanged {
        field: "value".into(),
    }));
    assert!(report.differences.contains(
        &ActorSchemaDifference::ChangedWithoutVersionBump { version: 1 }
    ));
    assert_eq!(report.migration_path, DeclaredMigrationPath::NotApplicable);
}

#[test]
fn complete_declared_chain_is_evidence_not_an_approval() {
    let before = schema("app::Counter", 1, PrimitiveType::Int);
    let mut after = schema("app::Counter", 3, PrimitiveType::String);
    after.migrations = vec![step(2, 3), step(1, 2)];

    let report = analyze_actor_schema_upgrade(&before, &after);
    assert!(!report.same_canonical_schema);
    assert_eq!(report.migration_path, DeclaredMigrationPath::Complete);
    assert!(report.differences.contains(&ActorSchemaDifference::FieldTypeChanged {
        field: "value".into(),
    }));
}

#[test]
fn missing_middle_migration_step_fails_closed() {
    let before = schema("app::Counter", 1, PrimitiveType::Int);
    let mut after = schema("app::Counter", 3, PrimitiveType::Int);
    after.migrations = vec![step(1, 2)];

    let report = analyze_actor_schema_upgrade(&before, &after);
    assert_eq!(
        report.migration_path,
        DeclaredMigrationPath::MissingStep {
            from_version: 2,
            to_version: 3,
        }
    );
}

#[test]
fn duplicate_and_skipping_migrations_are_not_considered_complete() {
    let before = schema("app::Counter", 1, PrimitiveType::Int);
    let mut after = schema("app::Counter", 3, PrimitiveType::Int);
    after.migrations = vec![step(1, 2), step(1, 2), step(2, 3)];

    assert_eq!(
        analyze_actor_schema_upgrade(&before, &after).migration_path,
        DeclaredMigrationPath::AmbiguousStep { from_version: 1 }
    );

    after.migrations = vec![step(1, 3)];
    assert_eq!(
        analyze_actor_schema_upgrade(&before, &after).migration_path,
        DeclaredMigrationPath::InvalidStep {
            from_version: 1,
            to_version: 3,
            expected_to_version: 2,
        }
    );
}

#[test]
fn renamed_actor_and_version_rollback_do_not_imply_compatibility() {
    let before = schema("app::Counter", 2, PrimitiveType::Int);
    let after = schema("other::Counter", 3, PrimitiveType::Int);
    let renamed = analyze_actor_schema_upgrade(&before, &after);
    assert!(renamed.differences.contains(&ActorSchemaDifference::ActorChanged {
        before: "app::Counter".into(),
        after: "other::Counter".into(),
    }));
    assert_eq!(renamed.migration_path, DeclaredMigrationPath::NotApplicable);

    let downgraded = analyze_actor_schema_upgrade(
        &before,
        &schema("app::Counter", 1, PrimitiveType::Int),
    );
    assert!(downgraded.differences.contains(&ActorSchemaDifference::VersionRegressed {
        before: 2,
        after: 1,
    }));
    assert_eq!(downgraded.migration_path, DeclaredMigrationPath::NotApplicable);
}

#[test]
fn additions_and_removals_are_visible_in_deterministic_order() {
    let mut before = schema("app::Counter", 1, PrimitiveType::Int);
    before.fields.push(ActorStateField {
        name: "removed".into(),
        ty: Type::Primitive(PrimitiveType::Bool),
    });
    let mut after = schema("app::Counter", 2, PrimitiveType::Int);
    after.fields.push(ActorStateField {
        name: "added".into(),
        ty: Type::Primitive(PrimitiveType::Bool),
    });
    after.migrations.push(step(1, 2));

    let report = analyze_actor_schema_upgrade(&before, &after);
    assert_eq!(
        report.differences,
        vec![
            ActorSchemaDifference::FieldRemoved {
                field: "removed".into(),
            },
            ActorSchemaDifference::FieldAdded {
                field: "added".into(),
            },
        ]
    );
    assert_eq!(report.migration_path, DeclaredMigrationPath::Complete);
}
