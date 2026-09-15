//! Backward-compatible schema-version encoding for durable persistence records.
//!
//! RFC 0008 requires the entity schema version to be recorded with snapshots
//! and journal records. Existing Nulang persistence files predate that field,
//! so an absent version must decode as schema version 1 rather than as 0 or an
//! error.
//!
//! This module deliberately keeps the version in the serialized record envelope
//! instead of adding it to the runtime's in-memory `ActorSnapshot`,
//! `JournalEntry`, and `EventEntry` structs. That lets the storage backends adopt
//! versioned encoding without forcing every in-memory constructor to know about
//! artifact/version policy.

use serde::de::DeserializeOwned;
use serde::Serialize;
use serde_json::{Map, Value};
use std::fmt;

use crate::migration_manifest::MigrationManifest;

pub const LEGACY_SCHEMA_VERSION: u32 = 1;
pub const SCHEMA_VERSION_FIELD: &str = "schema_version";

#[derive(Debug)]
pub enum PersistenceSchemaError {
    ZeroSchemaVersion,
    VersionOutOfRange(u64),
    InvalidVersionType,
    RecordMustBeObject,
    ReservedFieldCollision,
    AmbiguousActorMetadata(usize),
    InvalidMigrationManifest(String),
    ManifestVersionMismatch {
        actor_version: u32,
        manifest_version: u32,
    },
    Json(serde_json::Error),
}

impl fmt::Display for PersistenceSchemaError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::ZeroSchemaVersion => write!(f, "schema version must be >= 1"),
            Self::VersionOutOfRange(version) => {
                write!(f, "schema version {version} does not fit in u32")
            }
            Self::InvalidVersionType => write!(f, "schema_version must be a positive integer"),
            Self::RecordMustBeObject => {
                write!(f, "versioned persistence records must serialize as JSON objects")
            }
            Self::ReservedFieldCollision => write!(
                f,
                "record already contains reserved persistence field '{SCHEMA_VERSION_FIELD}'"
            ),
            Self::AmbiguousActorMetadata(count) => write!(
                f,
                "actor-local bytecode module contains {count} actor metadata records; schema identity is ambiguous"
            ),
            Self::InvalidMigrationManifest(error) => {
                write!(f, "invalid actor migration manifest: {error}")
            }
            Self::ManifestVersionMismatch {
                actor_version,
                manifest_version,
            } => write!(
                f,
                "actor schema version {actor_version} does not match migration manifest target version {manifest_version}"
            ),
            Self::Json(error) => write!(f, "persistence JSON error: {error}"),
        }
    }
}

impl std::error::Error for PersistenceSchemaError {
    fn source(&self) -> Option<&(dyn std::error::Error + 'static)> {
        match self {
            Self::Json(error) => Some(error),
            _ => None,
        }
    }
}

impl From<serde_json::Error> for PersistenceSchemaError {
    fn from(error: serde_json::Error) -> Self {
        Self::Json(error)
    }
}

#[derive(Debug, Clone, PartialEq)]
pub struct DecodedRecord<T> {
    pub schema_version: u32,
    pub record: T,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ActorSchemaIdentity {
    pub entity_type_name: Option<String>,
    pub schema_version: u32,
    pub migration_manifest_hash: Option<String>,
}

impl ActorSchemaIdentity {
    pub fn legacy_v1() -> Self {
        Self {
            entity_type_name: None,
            schema_version: LEGACY_SCHEMA_VERSION,
            migration_manifest_hash: None,
        }
    }
}

/// Resolve the authoritative schema identity attached to one actor.
///
/// Actor-local bytecode modules are projected to zero or one metadata record at
/// spawn/recovery registration. Ambiguous metadata is rejected rather than
/// guessing which entity type produced a persistence record.
pub fn actor_schema_identity(
    actor: &crate::runtime::Actor,
) -> Result<ActorSchemaIdentity, PersistenceSchemaError> {
    let Some(module) = actor.bytecode_module.as_ref() else {
        return Ok(ActorSchemaIdentity::legacy_v1());
    };
    match module.actor_metadata.as_slice() {
        [] => Ok(ActorSchemaIdentity::legacy_v1()),
        [meta] => {
            if meta.version == 0 {
                return Err(PersistenceSchemaError::ZeroSchemaVersion);
            }
            let migration_manifest_hash = if meta.migrations.trim().is_empty() {
                None
            } else {
                let manifest = MigrationManifest::from_json(&meta.migrations).map_err(|error| {
                    PersistenceSchemaError::InvalidMigrationManifest(error.to_string())
                })?;
                if manifest.target_version != meta.version {
                    return Err(PersistenceSchemaError::ManifestVersionMismatch {
                        actor_version: meta.version,
                        manifest_version: manifest.target_version,
                    });
                }
                Some(manifest.digest().map_err(|error| {
                    PersistenceSchemaError::InvalidMigrationManifest(error.to_string())
                })?)
            };
            Ok(ActorSchemaIdentity {
                entity_type_name: Some(meta.name.clone()),
                schema_version: meta.version,
                migration_manifest_hash,
            })
        }
        many => Err(PersistenceSchemaError::AmbiguousActorMetadata(many.len())),
    }
}

pub fn encode_record<T: Serialize>(
    record: &T,
    schema_version: u32,
) -> Result<String, PersistenceSchemaError> {
    if schema_version == 0 {
        return Err(PersistenceSchemaError::ZeroSchemaVersion);
    }
    let mut value = serde_json::to_value(record)?;
    let object = value
        .as_object_mut()
        .ok_or(PersistenceSchemaError::RecordMustBeObject)?;
    if object.contains_key(SCHEMA_VERSION_FIELD) {
        return Err(PersistenceSchemaError::ReservedFieldCollision);
    }
    object.insert(
        SCHEMA_VERSION_FIELD.to_string(),
        Value::Number(schema_version.into()),
    );
    Ok(serde_json::to_string(&value)?)
}

pub fn decode_record<T: DeserializeOwned>(
    json: &str,
) -> Result<DecodedRecord<T>, PersistenceSchemaError> {
    let mut value: Value = serde_json::from_str(json)?;
    let object = value
        .as_object_mut()
        .ok_or(PersistenceSchemaError::RecordMustBeObject)?;
    let schema_version = decode_version(object.remove(SCHEMA_VERSION_FIELD))?;
    let record = serde_json::from_value(value)?;
    Ok(DecodedRecord {
        schema_version,
        record,
    })
}

fn decode_version(value: Option<Value>) -> Result<u32, PersistenceSchemaError> {
    let Some(value) = value else {
        return Ok(LEGACY_SCHEMA_VERSION);
    };
    let Some(version) = value.as_u64() else {
        return Err(PersistenceSchemaError::InvalidVersionType);
    };
    if version == 0 {
        return Err(PersistenceSchemaError::ZeroSchemaVersion);
    }
    u32::try_from(version).map_err(|_| PersistenceSchemaError::VersionOutOfRange(version))
}

pub fn add_version_to_value(
    mut value: Value,
    schema_version: u32,
) -> Result<Value, PersistenceSchemaError> {
    if schema_version == 0 {
        return Err(PersistenceSchemaError::ZeroSchemaVersion);
    }
    let object: &mut Map<String, Value> = value
        .as_object_mut()
        .ok_or(PersistenceSchemaError::RecordMustBeObject)?;
    if object.contains_key(SCHEMA_VERSION_FIELD) {
        return Err(PersistenceSchemaError::ReservedFieldCollision);
    }
    object.insert(
        SCHEMA_VERSION_FIELD.to_string(),
        Value::Number(schema_version.into()),
    );
    Ok(value)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::migration_manifest::{
        MigrationContractMeta, MIGRATION_MANIFEST_FORMAT_VERSION,
    };
    use crate::runtime::{Actor, ActorSnapshot, EventEntry, JournalEntry, PersistedValue};

    #[test]
    fn legacy_snapshot_without_version_decodes_as_v1() {
        let legacy = r#"{
            "actor_id":7,
            "sequence":11,
            "state":{"count":{"tag":"Int","value":42}},
            "waiting_signal":null,
            "crdt_snapshot":null,
            "crdt_field_map":null
        }"#;
        let decoded: DecodedRecord<ActorSnapshot> = decode_record(legacy).unwrap();
        assert_eq!(decoded.schema_version, LEGACY_SCHEMA_VERSION);
        assert_eq!(decoded.record.actor_id, 7);
        assert_eq!(decoded.record.sequence, 11);
        assert_eq!(
            decoded.record.state.get("count"),
            Some(&PersistedValue::Int(42))
        );
    }

    #[test]
    fn snapshot_encoding_places_version_at_top_level() {
        let mut snapshot = ActorSnapshot::default();
        snapshot.actor_id = 9;
        snapshot.sequence = 3;
        let json = encode_record(&snapshot, 4).unwrap();
        let value: Value = serde_json::from_str(&json).unwrap();
        assert_eq!(value[SCHEMA_VERSION_FIELD], Value::Number(4u32.into()));
        let decoded: DecodedRecord<ActorSnapshot> = decode_record(&json).unwrap();
        assert_eq!(decoded.schema_version, 4);
        assert_eq!(decoded.record.actor_id, 9);
    }

    #[test]
    fn journal_entries_round_trip_with_origin_version() {
        let entry = JournalEntry {
            sequence: 5,
            behavior_id: 2,
            payload: vec![PersistedValue::Int(8)],
        };
        let json = encode_record(&entry, 3).unwrap();
        let decoded: DecodedRecord<JournalEntry> = decode_record(&json).unwrap();
        assert_eq!(decoded.schema_version, 3);
        assert_eq!(decoded.record.sequence, 5);
        assert_eq!(decoded.record.behavior_id, 2);
    }

    #[test]
    fn event_entries_round_trip_with_origin_version() {
        let entry = EventEntry {
            sequence: 6,
            field_name: "count".into(),
            event_name: "Incremented".into(),
            args: vec![PersistedValue::Int(1)],
            value: PersistedValue::Int(9),
        };
        let json = encode_record(&entry, 2).unwrap();
        let decoded: DecodedRecord<EventEntry> = decode_record(&json).unwrap();
        assert_eq!(decoded.schema_version, 2);
        assert_eq!(decoded.record.event_name, "Incremented");
        assert_eq!(decoded.record.value, PersistedValue::Int(9));
    }

    #[test]
    fn native_actor_defaults_to_legacy_v1_identity() {
        let actor = Actor::new(1, "native", 0);
        assert_eq!(
            actor_schema_identity(&actor).unwrap(),
            ActorSchemaIdentity::legacy_v1()
        );
    }

    #[test]
    fn actor_identity_comes_from_single_attached_metadata_record() {
        let manifest = MigrationManifest {
            format_version: MIGRATION_MANIFEST_FORMAT_VERSION,
            target_version: 2,
            contracts: vec![MigrationContractMeta {
                from_version: 1,
                to_version: 2,
                has_state_transform: true,
                event_transforms: Vec::new(),
            }],
        };
        let expected_hash = manifest.digest().unwrap();
        let mut meta = crate::bytecode::ActorMeta::new("Counter");
        meta.version = 2;
        meta.migrations = manifest.to_json().unwrap();
        let mut module = crate::bytecode::CodeModule::new("app");
        module.actor_metadata.push(meta);
        let mut actor = Actor::new(1, "actor_1", 0);
        actor.bytecode_module = Some(module);

        let identity = actor_schema_identity(&actor).unwrap();
        assert_eq!(identity.entity_type_name.as_deref(), Some("Counter"));
        assert_eq!(identity.schema_version, 2);
        assert_eq!(
            identity.migration_manifest_hash.as_deref(),
            Some(expected_hash.as_str())
        );
    }

    #[test]
    fn ambiguous_multi_entity_metadata_is_rejected() {
        let mut module = crate::bytecode::CodeModule::new("app");
        module
            .actor_metadata
            .push(crate::bytecode::ActorMeta::new("A"));
        module
            .actor_metadata
            .push(crate::bytecode::ActorMeta::new("B"));
        let mut actor = Actor::new(1, "actor_1", 0);
        actor.bytecode_module = Some(module);
        assert!(matches!(
            actor_schema_identity(&actor),
            Err(PersistenceSchemaError::AmbiguousActorMetadata(2))
        ));
    }

    #[test]
    fn manifest_target_must_match_actor_schema_version() {
        let manifest = MigrationManifest {
            format_version: MIGRATION_MANIFEST_FORMAT_VERSION,
            target_version: 2,
            contracts: vec![MigrationContractMeta {
                from_version: 1,
                to_version: 2,
                has_state_transform: false,
                event_transforms: Vec::new(),
            }],
        };
        let mut meta = crate::bytecode::ActorMeta::new("Counter");
        meta.version = 3;
        meta.migrations = manifest.to_json().unwrap();
        let mut module = crate::bytecode::CodeModule::new("app");
        module.actor_metadata.push(meta);
        let mut actor = Actor::new(1, "actor_1", 0);
        actor.bytecode_module = Some(module);
        assert!(matches!(
            actor_schema_identity(&actor),
            Err(PersistenceSchemaError::ManifestVersionMismatch {
                actor_version: 3,
                manifest_version: 2,
            })
        ));
    }

    #[test]
    fn explicit_zero_version_is_rejected() {
        let legacy = r#"{"schema_version":0,"sequence":1,"behavior_id":0,"payload":[]}"#;
        assert!(matches!(
            decode_record::<JournalEntry>(legacy),
            Err(PersistenceSchemaError::ZeroSchemaVersion)
        ));
    }

    #[test]
    fn non_integer_version_is_rejected() {
        let invalid =
            r#"{"schema_version":"2","sequence":1,"behavior_id":0,"payload":[]}"#;
        assert!(matches!(
            decode_record::<JournalEntry>(invalid),
            Err(PersistenceSchemaError::InvalidVersionType)
        ));
    }
}
