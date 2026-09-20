//! Backward-compatible schema-version encoding for durable persistence records.
//!
//! RFC 0008 requires durable state to retain the entity schema version that
//! produced it. Historical Nulang snapshots predate this metadata, so an absent
//! version is interpreted as schema version 1. The version lives in the
//! persistence envelope rather than in `ActorSnapshot` itself: schema provenance
//! is a storage/recovery concern, not ordinary actor state.

use serde::de::DeserializeOwned;
use serde::Serialize;
use serde_json::Value;
use std::fmt;

/// Schema version assigned to durable records written before RFC 0008 metadata.
pub const LEGACY_SCHEMA_VERSION: u32 = 1;
/// Stable top-level JSON field used for persistence schema provenance.
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

/// A decoded durable record and the entity schema version that produced it.
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

/// Deserialize a durable record. Missing `schema_version` means legacy v1.
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

#[cfg(test)]
mod tests {
    use super::*;

    #[derive(Debug, Clone, PartialEq, serde::Serialize, serde::Deserialize)]
    struct Record {
        value: u64,
    }

    #[test]
    fn legacy_record_without_version_decodes_as_v1() {
        let decoded: DecodedRecord<Record> = decode_record(r#"{"value":7}"#).unwrap();
        assert_eq!(decoded.schema_version, LEGACY_SCHEMA_VERSION);
        assert_eq!(decoded.record, Record { value: 7 });
    }

    #[test]
    fn explicit_version_round_trips() {
        let json = encode_record(&Record { value: 9 }, 4).unwrap();
        let decoded: DecodedRecord<Record> = decode_record(&json).unwrap();
        assert_eq!(decoded.schema_version, 4);
        assert_eq!(decoded.record, Record { value: 9 });
    }

    #[test]
    fn zero_version_is_rejected() {
        assert!(matches!(
            encode_record(&Record { value: 1 }, 0),
            Err(PersistenceSchemaError::ZeroSchemaVersion)
        ));
        assert!(matches!(
            decode_record::<Record>(r#"{"schema_version":0,"value":1}"#),
            Err(PersistenceSchemaError::ZeroSchemaVersion)
        ));
    }

    #[test]
    fn non_integer_version_is_rejected() {
        assert!(matches!(
            decode_record::<Record>(r#"{"schema_version":"2","value":1}"#),
            Err(PersistenceSchemaError::InvalidVersionType)
        ));
    }
}
