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

/// Schema version assigned to durable records written before RFC 0008 version
/// metadata existed.
pub const LEGACY_SCHEMA_VERSION: u32 = 1;
/// Stable JSON key used by snapshot and journal record encodings.
pub const SCHEMA_VERSION_FIELD: &str = "schema_version";

#[derive(Debug)]
pub enum PersistenceSchemaError {
    ZeroSchemaVersion,
    VersionOutOfRange(u64),
    InvalidVersionType,
    RecordMustBeObject,
    ReservedFieldCollision,
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

/// A decoded persistence record plus the schema version under which it was
/// written.
#[derive(Debug, Clone, PartialEq)]
pub struct DecodedRecord<T> {
    pub schema_version: u32,
    pub record: T,
}

/// Serialize a durable record with an explicit top-level `schema_version`.
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

/// Deserialize a durable record, treating an absent schema version as v1.
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

/// Insert a schema version into an already-serialized JSON object.
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
    use crate::runtime::{ActorSnapshot, EventEntry, JournalEntry, PersistedValue};

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