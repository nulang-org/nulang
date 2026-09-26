//! Durable event/snapshot schema evolution primitives.
//!
//! Event-sourced actors outlive the code version that first wrote their
//! journals. A durable runtime therefore needs explicit versioned envelopes,
//! deterministic upcasters, snapshot migration, and projection cursor rules.
//! This module is intentionally storage-backend agnostic: persistence engines
//! can serialize these envelopes without coupling migration policy to JSON
//! files, libSQL, Postgres, or an object store.

use serde::{Deserialize, Serialize};
use serde_json::Value;
use std::collections::{BTreeMap, HashMap};
use std::fmt;
use std::sync::Arc;

/// Schema versions start at 1. Zero is reserved for legacy/unversioned data.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash, Serialize, Deserialize)]
#[serde(transparent)]
pub struct SchemaVersion(u32);

impl SchemaVersion {
    pub const LEGACY: Self = Self(0);
    pub const V1: Self = Self(1);

    pub const fn new(value: u32) -> Self {
        Self(value)
    }

    pub const fn get(self) -> u32 {
        self.0
    }

    pub fn next(self) -> Result<Self, EvolutionError> {
        self.0
            .checked_add(1)
            .map(Self)
            .ok_or(EvolutionError::VersionOverflow)
    }
}

impl fmt::Display for SchemaVersion {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "v{}", self.0)
    }
}

/// Versioned event payload stored in a durable event journal.
///
/// `event_type` is the stable logical event name. Renaming implementation
/// functions must not implicitly rename persisted event types.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct VersionedEvent {
    pub event_type: String,
    pub schema_version: SchemaVersion,
    pub sequence: u64,
    pub payload: Value,
}

impl VersionedEvent {
    pub fn new(
        event_type: impl Into<String>,
        schema_version: SchemaVersion,
        sequence: u64,
        payload: Value,
    ) -> Self {
        Self {
            event_type: event_type.into(),
            schema_version,
            sequence,
            payload,
        }
    }
}

/// Versioned durable snapshot.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct VersionedSnapshot {
    pub schema: String,
    pub schema_version: SchemaVersion,
    pub sequence: u64,
    pub state: Value,
}

impl VersionedSnapshot {
    pub fn new(
        schema: impl Into<String>,
        schema_version: SchemaVersion,
        sequence: u64,
        state: Value,
    ) -> Self {
        Self {
            schema: schema.into(),
            schema_version,
            sequence,
            state,
        }
    }
}

/// Pure one-version migration function.
///
/// Migrations should be deterministic and side-effect free because recovery,
/// projection rebuilds, replicas, and debugging may execute them repeatedly.
pub type MigrationFn = Arc<dyn Fn(Value) -> Result<Value, String> + Send + Sync + 'static>;

#[derive(Clone)]
struct MigrationStep {
    migrate: MigrationFn,
}

impl fmt::Debug for MigrationStep {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("MigrationStep").finish_non_exhaustive()
    }
}

/// Registry of deterministic, contiguous schema migrations.
///
/// Each registered migration advances exactly one version: `vN -> vN+1`.
/// Requiring contiguous steps makes missing production migrations explicit and
/// avoids ambiguous shortest-path selection when several migration routes
/// exist.
#[derive(Debug, Clone, Default)]
pub struct MigrationRegistry {
    targets: HashMap<String, SchemaVersion>,
    steps: BTreeMap<(String, SchemaVersion), MigrationStep>,
}

impl MigrationRegistry {
    pub fn new() -> Self {
        Self::default()
    }

    /// Declare the schema version new writes should use for a logical type.
    pub fn set_target_version(
        &mut self,
        schema: impl Into<String>,
        target: SchemaVersion,
    ) -> Result<(), EvolutionError> {
        let schema = schema.into();
        if target == SchemaVersion::LEGACY {
            return Err(EvolutionError::LegacyCannotBeWriteTarget { schema });
        }
        self.targets.insert(schema, target);
        Ok(())
    }

    pub fn target_version(&self, schema: &str) -> Option<SchemaVersion> {
        self.targets.get(schema).copied()
    }

    /// Register one deterministic `from -> from+1` migration.
    pub fn register<F>(
        &mut self,
        schema: impl Into<String>,
        from: SchemaVersion,
        migrate: F,
    ) -> Result<(), EvolutionError>
    where
        F: Fn(Value) -> Result<Value, String> + Send + Sync + 'static,
    {
        let schema = schema.into();
        let to = from.next()?;
        let key = (schema.clone(), from);
        if self.steps.contains_key(&key) {
            return Err(EvolutionError::DuplicateMigration { schema, from, to });
        }
        self.steps.insert(
            key,
            MigrationStep {
                migrate: Arc::new(migrate),
            },
        );
        Ok(())
    }

    /// Validate that every declared schema can migrate every historic version
    /// in `[oldest_supported, target)` one step at a time.
    pub fn validate_chain(
        &self,
        schema: &str,
        oldest_supported: SchemaVersion,
    ) -> Result<(), EvolutionError> {
        let target = self
            .target_version(schema)
            .ok_or_else(|| EvolutionError::UnknownSchema {
                schema: schema.to_string(),
            })?;
        if oldest_supported > target {
            return Err(EvolutionError::FutureVersion {
                schema: schema.to_string(),
                found: oldest_supported,
                target,
            });
        }

        let mut current = oldest_supported;
        while current < target {
            if !self.steps.contains_key(&(schema.to_string(), current)) {
                return Err(EvolutionError::MissingMigration {
                    schema: schema.to_string(),
                    from: current,
                    to: current.next()?,
                });
            }
            current = current.next()?;
        }
        Ok(())
    }

    /// Upcast a payload to the declared target version.
    pub fn upcast(
        &self,
        schema: &str,
        from: SchemaVersion,
        payload: Value,
    ) -> Result<(SchemaVersion, Value), EvolutionError> {
        let target = self
            .target_version(schema)
            .ok_or_else(|| EvolutionError::UnknownSchema {
                schema: schema.to_string(),
            })?;

        if from > target {
            return Err(EvolutionError::FutureVersion {
                schema: schema.to_string(),
                found: from,
                target,
            });
        }

        let mut version = from;
        let mut value = payload;
        while version < target {
            let to = version.next()?;
            let step = self
                .steps
                .get(&(schema.to_string(), version))
                .ok_or_else(|| EvolutionError::MissingMigration {
                    schema: schema.to_string(),
                    from: version,
                    to,
                })?;
            value = (step.migrate)(value).map_err(|message| EvolutionError::MigrationFailed {
                schema: schema.to_string(),
                from: version,
                to,
                message,
            })?;
            version = to;
        }
        Ok((version, value))
    }

    pub fn upcast_event(
        &self,
        mut event: VersionedEvent,
    ) -> Result<VersionedEvent, EvolutionError> {
        let (version, payload) =
            self.upcast(&event.event_type, event.schema_version, event.payload)?;
        event.schema_version = version;
        event.payload = payload;
        Ok(event)
    }

    pub fn upcast_snapshot(
        &self,
        mut snapshot: VersionedSnapshot,
    ) -> Result<VersionedSnapshot, EvolutionError> {
        let (version, state) =
            self.upcast(&snapshot.schema, snapshot.schema_version, snapshot.state)?;
        snapshot.schema_version = version;
        snapshot.state = state;
        Ok(snapshot)
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum EvolutionError {
    UnknownSchema {
        schema: String,
    },
    LegacyCannotBeWriteTarget {
        schema: String,
    },
    FutureVersion {
        schema: String,
        found: SchemaVersion,
        target: SchemaVersion,
    },
    DuplicateMigration {
        schema: String,
        from: SchemaVersion,
        to: SchemaVersion,
    },
    MissingMigration {
        schema: String,
        from: SchemaVersion,
        to: SchemaVersion,
    },
    MigrationFailed {
        schema: String,
        from: SchemaVersion,
        to: SchemaVersion,
        message: String,
    },
    VersionOverflow,
}

impl fmt::Display for EvolutionError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            EvolutionError::UnknownSchema { schema } => {
                write!(f, "unknown durable schema '{schema}'")
            }
            EvolutionError::LegacyCannotBeWriteTarget { schema } => {
                write!(
                    f,
                    "schema '{schema}' cannot use legacy version 0 as a write target"
                )
            }
            EvolutionError::FutureVersion {
                schema,
                found,
                target,
            } => write!(
                f,
                "schema '{schema}' is {found}, newer than runtime target {target}"
            ),
            EvolutionError::DuplicateMigration { schema, from, to } => {
                write!(f, "duplicate migration for '{schema}' {from} -> {to}")
            }
            EvolutionError::MissingMigration { schema, from, to } => {
                write!(f, "missing migration for '{schema}' {from} -> {to}")
            }
            EvolutionError::MigrationFailed {
                schema,
                from,
                to,
                message,
            } => write!(
                f,
                "migration for '{schema}' {from} -> {to} failed: {message}"
            ),
            EvolutionError::VersionOverflow => write!(f, "schema version overflow"),
        }
    }
}

impl std::error::Error for EvolutionError {}

/// Durable projection cursor.
///
/// Projection rebuilds must be deterministic and gap-aware. A duplicate or
/// replayed event at/below `last_sequence` is already applied; exactly the next
/// sequence advances; a jump indicates missing journal data and fails closed.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ProjectionCursor {
    pub projection: String,
    pub last_sequence: u64,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ProjectionAdvance {
    AlreadyApplied,
    Applied,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ProjectionGap {
    pub projection: String,
    pub expected_sequence: u64,
    pub actual_sequence: u64,
}

impl fmt::Display for ProjectionGap {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(
            f,
            "projection '{}' expected sequence {}, got {}",
            self.projection, self.expected_sequence, self.actual_sequence
        )
    }
}

impl std::error::Error for ProjectionGap {}

impl ProjectionCursor {
    pub fn new(projection: impl Into<String>) -> Self {
        Self {
            projection: projection.into(),
            last_sequence: 0,
        }
    }

    pub fn from_sequence(projection: impl Into<String>, last_sequence: u64) -> Self {
        Self {
            projection: projection.into(),
            last_sequence,
        }
    }

    pub fn advance(&mut self, sequence: u64) -> Result<ProjectionAdvance, ProjectionGap> {
        if sequence <= self.last_sequence {
            return Ok(ProjectionAdvance::AlreadyApplied);
        }

        let expected = self.last_sequence.saturating_add(1);
        if sequence != expected {
            return Err(ProjectionGap {
                projection: self.projection.clone(),
                expected_sequence: expected,
                actual_sequence: sequence,
            });
        }

        self.last_sequence = sequence;
        Ok(ProjectionAdvance::Applied)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    fn registry() -> MigrationRegistry {
        let mut registry = MigrationRegistry::new();
        registry
            .set_target_version("UserCreated", SchemaVersion::new(3))
            .unwrap();
        registry
            .register("UserCreated", SchemaVersion::new(1), |mut payload| {
                let object = payload
                    .as_object_mut()
                    .ok_or_else(|| "expected object".to_string())?;
                let name = object
                    .remove("name")
                    .ok_or_else(|| "missing name".to_string())?;
                object.insert("display_name".into(), name);
                Ok(payload)
            })
            .unwrap();
        registry
            .register("UserCreated", SchemaVersion::new(2), |mut payload| {
                let object = payload
                    .as_object_mut()
                    .ok_or_else(|| "expected object".to_string())?;
                object.insert("active".into(), Value::Bool(true));
                Ok(payload)
            })
            .unwrap();
        registry
    }

    #[test]
    fn upcasts_event_through_contiguous_chain() {
        let event = VersionedEvent::new(
            "UserCreated",
            SchemaVersion::new(1),
            42,
            json!({"name": "Ada"}),
        );
        let upgraded = registry().upcast_event(event).unwrap();
        assert_eq!(upgraded.schema_version, SchemaVersion::new(3));
        assert_eq!(upgraded.sequence, 42);
        assert_eq!(upgraded.payload["display_name"], json!("Ada"));
        assert_eq!(upgraded.payload["active"], json!(true));
        assert!(upgraded.payload.get("name").is_none());
    }

    #[test]
    fn snapshot_uses_same_deterministic_migration_engine() {
        let snapshot = VersionedSnapshot::new(
            "UserCreated",
            SchemaVersion::new(2),
            100,
            json!({"display_name": "Grace"}),
        );
        let upgraded = registry().upcast_snapshot(snapshot).unwrap();
        assert_eq!(upgraded.schema_version, SchemaVersion::new(3));
        assert_eq!(upgraded.sequence, 100);
        assert_eq!(upgraded.state["active"], json!(true));
    }

    #[test]
    fn future_data_fails_closed() {
        let err = registry()
            .upcast(
                "UserCreated",
                SchemaVersion::new(4),
                json!({"display_name": "Future"}),
            )
            .unwrap_err();
        assert_eq!(
            err,
            EvolutionError::FutureVersion {
                schema: "UserCreated".into(),
                found: SchemaVersion::new(4),
                target: SchemaVersion::new(3),
            }
        );
    }

    #[test]
    fn missing_migration_is_detected_before_recovery() {
        let mut registry = MigrationRegistry::new();
        registry
            .set_target_version("Invoice", SchemaVersion::new(3))
            .unwrap();
        registry
            .register("Invoice", SchemaVersion::new(1), Ok)
            .unwrap();

        assert_eq!(
            registry
                .validate_chain("Invoice", SchemaVersion::new(1))
                .unwrap_err(),
            EvolutionError::MissingMigration {
                schema: "Invoice".into(),
                from: SchemaVersion::new(2),
                to: SchemaVersion::new(3),
            }
        );
    }

    #[test]
    fn duplicate_migration_registration_is_rejected() {
        let mut registry = MigrationRegistry::new();
        registry
            .register("Order", SchemaVersion::new(1), Ok)
            .unwrap();
        assert_eq!(
            registry
                .register("Order", SchemaVersion::new(1), Ok)
                .unwrap_err(),
            EvolutionError::DuplicateMigration {
                schema: "Order".into(),
                from: SchemaVersion::new(1),
                to: SchemaVersion::new(2),
            }
        );
    }

    #[test]
    fn migration_failure_identifies_exact_schema_step() {
        let mut registry = MigrationRegistry::new();
        registry
            .set_target_version("Payment", SchemaVersion::new(2))
            .unwrap();
        registry
            .register("Payment", SchemaVersion::new(1), |_payload| {
                Err("currency missing".into())
            })
            .unwrap();
        let err = registry
            .upcast("Payment", SchemaVersion::new(1), json!({}))
            .unwrap_err();
        assert_eq!(
            err,
            EvolutionError::MigrationFailed {
                schema: "Payment".into(),
                from: SchemaVersion::new(1),
                to: SchemaVersion::new(2),
                message: "currency missing".into(),
            }
        );
    }

    #[test]
    fn projection_cursor_is_duplicate_tolerant_but_gap_intolerant() {
        let mut cursor = ProjectionCursor::new("billing_totals");
        assert_eq!(cursor.advance(1).unwrap(), ProjectionAdvance::Applied);
        assert_eq!(
            cursor.advance(1).unwrap(),
            ProjectionAdvance::AlreadyApplied
        );
        assert_eq!(
            cursor.advance(0).unwrap(),
            ProjectionAdvance::AlreadyApplied
        );
        assert_eq!(
            cursor.advance(3).unwrap_err(),
            ProjectionGap {
                projection: "billing_totals".into(),
                expected_sequence: 2,
                actual_sequence: 3,
            }
        );
        assert_eq!(cursor.last_sequence, 1);
        assert_eq!(cursor.advance(2).unwrap(), ProjectionAdvance::Applied);
    }
}
