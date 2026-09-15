//! Durable single-assignment promises (RFC 0020 foundation).
//!
//! This module intentionally implements the persistence/state-machine layer
//! before language syntax or VM suspension. A durable promise has a stable ID,
//! begins Pending, and may transition exactly once to Resolved, Rejected, or
//! Cancelled. Repeating the *same* terminal completion is idempotent; attempting
//! a different completion is rejected.

use std::collections::HashMap;
use std::fs;
use std::io::{self, Write};
use std::path::{Path, PathBuf};

use serde::{Deserialize, Serialize};

use crate::runtime::PersistedValue;

pub const DURABLE_PROMISE_FORMAT_VERSION: u16 = 1;

/// Stable promise identity. Cloud implementations may encode tenant/region
/// information externally; core semantics treat the ID as opaque.
#[derive(Debug, Clone, PartialEq, Eq, Hash, Serialize, Deserialize)]
pub struct PromiseId(pub String);

impl PromiseId {
    pub fn new(id: impl Into<String>) -> Result<Self, PromiseError> {
        let id = id.into();
        if id.trim().is_empty() {
            return Err(PromiseError::InvalidId);
        }
        Ok(Self(id))
    }
}

impl std::fmt::Display for PromiseId {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(&self.0)
    }
}

/// Durable promise state. Terminal states are immutable.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(tag = "state", content = "value", rename_all = "snake_case")]
pub enum PromiseState {
    Pending,
    Resolved(PersistedValue),
    Rejected(String),
    Cancelled(String),
}

impl PromiseState {
    pub fn is_terminal(&self) -> bool {
        !matches!(self, Self::Pending)
    }
}

/// A caller's attempt to complete a promise.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(tag = "completion", content = "value", rename_all = "snake_case")]
pub enum PromiseCompletion {
    Resolve(PersistedValue),
    Reject(String),
    Cancel(String),
}

impl PromiseCompletion {
    fn into_state(self) -> PromiseState {
        match self {
            Self::Resolve(value) => PromiseState::Resolved(value),
            Self::Reject(error) => PromiseState::Rejected(error),
            Self::Cancel(reason) => PromiseState::Cancelled(reason),
        }
    }
}

/// Append-only completion event. `sequence` is local to one promise.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct PromiseEvent {
    pub sequence: u64,
    pub promise_id: PromiseId,
    pub state: PromiseState,
    /// Principal/workload/resolver identity supplied by the caller.
    pub completed_by: Option<String>,
}

/// Persisted representation of one promise.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct PromiseRecord {
    pub format_version: u16,
    pub id: PromiseId,
    pub state: PromiseState,
    #[serde(default)]
    pub events: Vec<PromiseEvent>,
}

impl PromiseRecord {
    pub fn pending(id: PromiseId) -> Self {
        Self {
            format_version: DURABLE_PROMISE_FORMAT_VERSION,
            id,
            state: PromiseState::Pending,
            events: Vec::new(),
        }
    }

    fn validate(&self) -> Result<(), PromiseError> {
        if self.format_version != DURABLE_PROMISE_FORMAT_VERSION {
            return Err(PromiseError::UnsupportedFormat(self.format_version));
        }
        if self.id.0.trim().is_empty() {
            return Err(PromiseError::InvalidId);
        }
        Ok(())
    }
}

/// Result of a completion attempt.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum CompletionOutcome {
    /// Pending -> terminal transition was persisted.
    Applied,
    /// The promise was already in the identical terminal state. No new event
    /// was appended; callers may safely retry resolution after transport loss.
    IdempotentReplay,
}

#[derive(Debug)]
pub enum PromiseError {
    InvalidId,
    AlreadyExists(PromiseId),
    NotFound(PromiseId),
    Conflict {
        id: PromiseId,
        existing: PromiseState,
        attempted: PromiseState,
    },
    UnsupportedFormat(u16),
    Serialization(String),
    Io(io::Error),
}

impl std::fmt::Display for PromiseError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::InvalidId => write!(f, "promise id must not be empty"),
            Self::AlreadyExists(id) => write!(f, "promise '{id}' already exists"),
            Self::NotFound(id) => write!(f, "promise '{id}' not found"),
            Self::Conflict {
                id,
                existing,
                attempted,
            } => write!(
                f,
                "promise '{id}' is already completed as {existing:?}; conflicting completion {attempted:?} rejected"
            ),
            Self::UnsupportedFormat(v) => write!(f, "unsupported durable-promise format version {v}"),
            Self::Serialization(msg) => write!(f, "durable-promise serialization failed: {msg}"),
            Self::Io(err) => write!(f, "durable-promise IO error: {err}"),
        }
    }
}

impl std::error::Error for PromiseError {
    fn source(&self) -> Option<&(dyn std::error::Error + 'static)> {
        match self {
            Self::Io(err) => Some(err),
            _ => None,
        }
    }
}

impl From<io::Error> for PromiseError {
    fn from(value: io::Error) -> Self {
        Self::Io(value)
    }
}

/// Storage contract used by the runtime and Cloud implementations.
pub trait DurablePromiseStore: Send + Sync {
    fn create(&mut self, id: PromiseId) -> Result<PromiseRecord, PromiseError>;
    fn load(&self, id: &PromiseId) -> Result<Option<PromiseRecord>, PromiseError>;
    fn complete(
        &mut self,
        id: &PromiseId,
        completion: PromiseCompletion,
        completed_by: Option<String>,
    ) -> Result<CompletionOutcome, PromiseError>;

    fn state(&self, id: &PromiseId) -> Result<PromiseState, PromiseError> {
        self.load(id)?
            .map(|record| record.state)
            .ok_or_else(|| PromiseError::NotFound(id.clone()))
    }

    fn events(&self, id: &PromiseId) -> Result<Vec<PromiseEvent>, PromiseError> {
        self.load(id)?
            .map(|record| record.events)
            .ok_or_else(|| PromiseError::NotFound(id.clone()))
    }
}

/// Apply the single-assignment transition to one loaded record.
fn apply_completion(
    record: &mut PromiseRecord,
    completion: PromiseCompletion,
    completed_by: Option<String>,
) -> Result<CompletionOutcome, PromiseError> {
    record.validate()?;
    let attempted = completion.into_state();
    match &record.state {
        PromiseState::Pending => {
            let sequence = record.events.last().map(|e| e.sequence + 1).unwrap_or(1);
            record.state = attempted.clone();
            record.events.push(PromiseEvent {
                sequence,
                promise_id: record.id.clone(),
                state: attempted,
                completed_by,
            });
            Ok(CompletionOutcome::Applied)
        }
        existing if existing == &attempted => Ok(CompletionOutcome::IdempotentReplay),
        existing => Err(PromiseError::Conflict {
            id: record.id.clone(),
            existing: existing.clone(),
            attempted,
        }),
    }
}

/// In-memory durable-promise store for tests/embedded runtimes.
#[derive(Debug, Default, Clone)]
pub struct MemoryPromiseStore {
    records: HashMap<PromiseId, PromiseRecord>,
}

impl MemoryPromiseStore {
    pub fn new() -> Self {
        Self::default()
    }
}

impl DurablePromiseStore for MemoryPromiseStore {
    fn create(&mut self, id: PromiseId) -> Result<PromiseRecord, PromiseError> {
        if self.records.contains_key(&id) {
            return Err(PromiseError::AlreadyExists(id));
        }
        let record = PromiseRecord::pending(id.clone());
        self.records.insert(id, record.clone());
        Ok(record)
    }

    fn load(&self, id: &PromiseId) -> Result<Option<PromiseRecord>, PromiseError> {
        Ok(self.records.get(id).cloned())
    }

    fn complete(
        &mut self,
        id: &PromiseId,
        completion: PromiseCompletion,
        completed_by: Option<String>,
    ) -> Result<CompletionOutcome, PromiseError> {
        let record = self
            .records
            .get_mut(id)
            .ok_or_else(|| PromiseError::NotFound(id.clone()))?;
        apply_completion(record, completion, completed_by)
    }
}

/// Simple JSON-file backend. Each promise is stored independently under a
/// BLAKE3-derived filename so opaque IDs cannot escape the store directory.
/// Writes use temp-file + rename replacement.
#[derive(Debug, Clone)]
pub struct JsonFilePromiseStore {
    root: PathBuf,
}

impl JsonFilePromiseStore {
    pub fn new(root: impl Into<PathBuf>) -> Result<Self, PromiseError> {
        let root = root.into();
        fs::create_dir_all(&root)?;
        Ok(Self { root })
    }

    fn path_for(&self, id: &PromiseId) -> PathBuf {
        let digest = blake3::hash(id.0.as_bytes()).to_hex().to_string();
        self.root.join(format!("promise-{digest}.json"))
    }

    fn write_record(&self, record: &PromiseRecord) -> Result<(), PromiseError> {
        record.validate()?;
        let path = self.path_for(&record.id);
        let tmp = path.with_extension("json.tmp");
        let bytes = serde_json::to_vec_pretty(record)
            .map_err(|e| PromiseError::Serialization(e.to_string()))?;
        {
            let mut file = fs::File::create(&tmp)?;
            file.write_all(&bytes)?;
            file.sync_all()?;
        }
        fs::rename(tmp, path)?;
        Ok(())
    }

    fn read_record(&self, id: &PromiseId) -> Result<Option<PromiseRecord>, PromiseError> {
        let path = self.path_for(id);
        if !path.exists() {
            return Ok(None);
        }
        let bytes = fs::read(path)?;
        let record: PromiseRecord = serde_json::from_slice(&bytes)
            .map_err(|e| PromiseError::Serialization(e.to_string()))?;
        record.validate()?;
        if &record.id != id {
            return Err(PromiseError::Serialization(
                "promise file identity does not match requested id".into(),
            ));
        }
        Ok(Some(record))
    }

    pub fn root(&self) -> &Path {
        &self.root
    }
}

impl DurablePromiseStore for JsonFilePromiseStore {
    fn create(&mut self, id: PromiseId) -> Result<PromiseRecord, PromiseError> {
        if self.read_record(&id)?.is_some() {
            return Err(PromiseError::AlreadyExists(id));
        }
        let record = PromiseRecord::pending(id);
        self.write_record(&record)?;
        Ok(record)
    }

    fn load(&self, id: &PromiseId) -> Result<Option<PromiseRecord>, PromiseError> {
        self.read_record(id)
    }

    fn complete(
        &mut self,
        id: &PromiseId,
        completion: PromiseCompletion,
        completed_by: Option<String>,
    ) -> Result<CompletionOutcome, PromiseError> {
        let mut record = self
            .read_record(id)?
            .ok_or_else(|| PromiseError::NotFound(id.clone()))?;
        let outcome = apply_completion(&mut record, completion, completed_by)?;
        if outcome == CompletionOutcome::Applied {
            self.write_record(&record)?;
        }
        Ok(outcome)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use tempfile::TempDir;

    fn id() -> PromiseId {
        PromiseId::new("tenant-a/order-42/approval").unwrap()
    }

    #[test]
    fn promise_starts_pending_and_resolves_once() {
        let mut store = MemoryPromiseStore::new();
        store.create(id()).unwrap();
        assert_eq!(store.state(&id()).unwrap(), PromiseState::Pending);
        assert_eq!(
            store
                .complete(
                    &id(),
                    PromiseCompletion::Resolve(PersistedValue::String("approved".into())),
                    Some("reviewer:7".into()),
                )
                .unwrap(),
            CompletionOutcome::Applied
        );
        assert_eq!(
            store.state(&id()).unwrap(),
            PromiseState::Resolved(PersistedValue::String("approved".into()))
        );
        assert_eq!(store.events(&id()).unwrap().len(), 1);
    }

    #[test]
    fn identical_retry_is_idempotent_and_does_not_append_event() {
        let mut store = MemoryPromiseStore::new();
        store.create(id()).unwrap();
        let completion = PromiseCompletion::Resolve(PersistedValue::Int(7));
        assert_eq!(
            store
                .complete(&id(), completion.clone(), Some("resolver-a".into()))
                .unwrap(),
            CompletionOutcome::Applied
        );
        assert_eq!(
            store
                .complete(&id(), completion, Some("resolver-b".into()))
                .unwrap(),
            CompletionOutcome::IdempotentReplay
        );
        assert_eq!(store.events(&id()).unwrap().len(), 1);
    }

    #[test]
    fn conflicting_terminal_completion_is_rejected() {
        let mut store = MemoryPromiseStore::new();
        store.create(id()).unwrap();
        store
            .complete(
                &id(),
                PromiseCompletion::Resolve(PersistedValue::Bool(true)),
                None,
            )
            .unwrap();
        let err = store
            .complete(
                &id(),
                PromiseCompletion::Reject("late failure".into()),
                None,
            )
            .unwrap_err();
        assert!(matches!(err, PromiseError::Conflict { .. }));
    }

    #[test]
    fn rejection_and_cancellation_are_terminal() {
        let mut rejected = MemoryPromiseStore::new();
        rejected.create(id()).unwrap();
        rejected
            .complete(&id(), PromiseCompletion::Reject("denied".into()), None)
            .unwrap();
        assert_eq!(
            rejected.state(&id()).unwrap(),
            PromiseState::Rejected("denied".into())
        );

        let cancel_id = PromiseId::new("cancel-me").unwrap();
        let mut cancelled = MemoryPromiseStore::new();
        cancelled.create(cancel_id.clone()).unwrap();
        cancelled
            .complete(
                &cancel_id,
                PromiseCompletion::Cancel("timeout".into()),
                None,
            )
            .unwrap();
        assert!(cancelled.state(&cancel_id).unwrap().is_terminal());
    }

    #[test]
    fn json_store_survives_reopen_and_replays_terminal_state() {
        let dir = TempDir::new().unwrap();
        {
            let mut store = JsonFilePromiseStore::new(dir.path()).unwrap();
            store.create(id()).unwrap();
            store
                .complete(
                    &id(),
                    PromiseCompletion::Resolve(PersistedValue::Int(99)),
                    Some("external:webhook".into()),
                )
                .unwrap();
        }
        let store = JsonFilePromiseStore::new(dir.path()).unwrap();
        assert_eq!(
            store.state(&id()).unwrap(),
            PromiseState::Resolved(PersistedValue::Int(99))
        );
        let events = store.events(&id()).unwrap();
        assert_eq!(events.len(), 1);
        assert_eq!(events[0].sequence, 1);
        assert_eq!(events[0].completed_by.as_deref(), Some("external:webhook"));
    }

    #[test]
    fn opaque_id_cannot_escape_file_store_root() {
        let dir = TempDir::new().unwrap();
        let mut store = JsonFilePromiseStore::new(dir.path()).unwrap();
        let dangerous = PromiseId::new("../../outside").unwrap();
        store.create(dangerous.clone()).unwrap();
        assert_eq!(store.state(&dangerous).unwrap(), PromiseState::Pending);
        let entries: Vec<_> = fs::read_dir(dir.path()).unwrap().collect();
        assert_eq!(entries.len(), 1);
    }
}
