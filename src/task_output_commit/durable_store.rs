//! Fenced task-output publication through the existing atomic actor journal.
//!
//! Each logical task occupies a dedicated, collision-free actor slot ID. The
//! adapter persists a complete versioned task checkpoint as one domain event
//! in a `DurableTransition`. The transition's predecessor sequence is a CAS
//! guard and its activation epoch is the task's authoritative fencing epoch.
//!
//! WARNING: Only a trusted controller may assign attempts after fencing prior
//! ownership; no network credential or lease proof is established here.
//! Artifact bytes and tenant/object authorization are verified by the host,
//! not this adapter. Do not use a slot shared with ordinary actor activities.

use super::{AttemptToken, CommitDecision, CommitError, TaskOutputState};
use crate::runtime::{
    DurableTransition, EventEntry, PersistedValue, PersistenceStore, DURABLE_TRANSITION_VERSION,
};
use std::fmt;
use std::io;

const FIELD: &str = "__nulang.task_output.state.v1";
const EVENT: &str = "TaskOutputCheckpoint";

#[derive(Debug)]
pub enum TaskOutputStoreError {
    Storage(io::Error),
    Semantic(CommitError),
    Corrupt(String),
    InvalidSlot,
}

impl fmt::Display for TaskOutputStoreError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Storage(error) => write!(f, "task output storage: {error}"),
            Self::Semantic(error) => error.fmt(f),
            Self::Corrupt(reason) => write!(f, "invalid durable task output history: {reason}"),
            Self::InvalidSlot => write!(f, "task output requires a dedicated positive i64 actor slot"),
        }
    }
}

impl std::error::Error for TaskOutputStoreError {}

impl From<io::Error> for TaskOutputStoreError {
    fn from(error: io::Error) -> Self {
        Self::Storage(error)
    }
}

impl From<CommitError> for TaskOutputStoreError {
    fn from(error: CommitError) -> Self {
        Self::Semantic(error)
    }
}

pub struct DurableTaskOutputStore<'a> {
    store: &'a mut dyn PersistenceStore,
    slot: u64,
    task_key: String,
}

impl<'a> DurableTaskOutputStore<'a> {
    /// The slot must be globally unique for this logical task within a
    /// persistence namespace; collisions fail closed on recovery.
    pub fn new(
        store: &'a mut dyn PersistenceStore,
        slot: u64,
        task_key: impl Into<String>,
    ) -> Result<Self, TaskOutputStoreError> {
        if slot == 0 || slot > i64::MAX as u64 {
            return Err(TaskOutputStoreError::InvalidSlot);
        }
        let task_key = task_key.into();
        TaskOutputState::new(task_key.clone())?;
        Ok(Self { store, slot, task_key })
    }

    /// Recover only an unambiguous, complete series of atomic task checkpoints.
    ///
    /// Recovered sequences MUST match the authoritative durable tail without
    /// gaps. `read_events` may be lossy in legacy backends; gaps, foreign
    /// events, future formats, and duplicate sequences are rejected.
    fn load_versioned(&self) -> Result<(u64, TaskOutputState), TaskOutputStoreError> {
        let tail = self.store.load_durable_tail_position(self.slot)?;
        let events = self.store.read_events(self.slot);

        let Some(tail) = tail else {
            if !events.is_empty() || self.store.latest_sequence(self.slot) != 0 {
                return Err(TaskOutputStoreError::Corrupt(
                    "task slot contains non-atomic or unrelated history".into(),
                ));
            }
            return Ok((0, TaskOutputState::new(self.task_key.clone())?));
        };

        if tail.sequence == 0
            || tail.sequence != events.len() as u64
            || tail.activation_epoch == 0
        {
            return Err(TaskOutputStoreError::Corrupt(
                "missing, duplicated, or truncated task checkpoint stream".into(),
            ));
        }

        let mut recovered = None;
        for (ordinal, event) in events.iter().enumerate() {
            let expected_sequence = ordinal as u64 + 1;
            if event.sequence != expected_sequence
                || event.field_name != FIELD
                || event.event_name != EVENT
                || !event.args.is_empty()
            {
                return Err(TaskOutputStoreError::Corrupt(
                    "foreign, out-of-order, or malformed task history".into(),
                ));
            }
            let PersistedValue::String(json) = &event.value else {
                return Err(TaskOutputStoreError::Corrupt(
                    "checkpoint payload is not a persistent string".into(),
                ));
            };
            // Validate every checkpoint rather than trusting just the last one;
            // a partially corrupted history must not silently resurrect an owner.
            let state = TaskOutputState::restore(json.as_bytes())
                .map_err(|error| TaskOutputStoreError::Corrupt(error.to_string()))?;
            if state.task_key() != self.task_key {
                return Err(TaskOutputStoreError::Corrupt(
                    "task key differs from durable slot identity".into(),
                ));
            }
            recovered = Some(state);
        }

        let state = recovered.ok_or_else(|| {
            TaskOutputStoreError::Corrupt("durable tail exists without a checkpoint".into())
        })?;
        if state.current_epoch() != tail.activation_epoch {
            return Err(TaskOutputStoreError::Corrupt(
                "task checkpoint epoch disagrees with durable fencing tail".into(),
            ));
        }
        Ok((tail.sequence, state))
    }

    pub fn load(&self) -> Result<TaskOutputState, TaskOutputStoreError> {
        Ok(self.load_versioned()?.1)
    }

    fn update<R>(
        &mut self,
        mutate: impl FnOnce(&mut TaskOutputState) -> Result<R, CommitError>,
    ) -> Result<R, TaskOutputStoreError> {
        let (previous_sequence, mut state) = self.load_versioned()?;
        let previous = state.clone();
        let outcome = mutate(&mut state)?;
        if state == previous {
            return Ok(outcome);
        }
        if state.current_epoch() == 0 || state.current_epoch() > i64::MAX as u64 {
            return Err(TaskOutputStoreError::Corrupt(
                "invalid epoch for a durable transition".into(),
            ));
        }
        let sequence = previous_sequence.checked_add(1).ok_or_else(|| {
            TaskOutputStoreError::Corrupt("task journal sequence exhausted".into())
        })?;
        let checkpoint = state.checkpoint()?;
        let json = String::from_utf8(checkpoint).map_err(|error| {
            TaskOutputStoreError::Corrupt(format!("invalid UTF-8 checkpoint: {error}"))
        })?;
        let transition = DurableTransition {
            version: DURABLE_TRANSITION_VERSION,
            actor_id: self.slot,
            activation_epoch: state.current_epoch(),
            sequence,
            expected_previous_sequence: previous_sequence,
            command: None,
            snapshot: None,
            workflow_events: vec![],
            domain_events: vec![EventEntry {
                sequence,
                field_name: FIELD.into(),
                event_name: EVENT.into(),
                args: vec![],
                value: PersistedValue::String(json),
            }],
            durable_effects: vec![],
            outbox: vec![],
        };
        // This is the only publication point. Never retry a losing CAS using
        // the stale transformed state: callers must reload and reauthorize.
        self.store.commit_transition(transition)?;
        Ok(outcome)
    }

    /// Trusted-controller operation. A retry with the exact current attempt
    /// ID is a no-op. A new ID supersedes any prior staged-but-uncommitted
    /// attempt and increments the durable fencing epoch.
    pub fn assign(&mut self, attempt_id: &str) -> Result<AttemptToken, TaskOutputStoreError> {
        self.update(|state| state.assign(attempt_id))
    }

    /// Records an immutable staging reference; does not expose the result.
    pub fn stage(
        &mut self,
        owner: &AttemptToken,
        artifact_key: &str,
        digest: [u8; 32],
    ) -> Result<(), TaskOutputStoreError> {
        self.update(|state| state.stage(owner, artifact_key, digest))
    }

    /// Publishes a verified staged result through the atomic durable journal.
    pub fn commit(
        &mut self,
        owner: &AttemptToken,
        digest: [u8; 32],
    ) -> Result<CommitDecision, TaskOutputStoreError> {
        self.update(|state| state.commit(owner, digest))
    }
}
