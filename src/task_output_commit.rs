//! Experimental, storage-neutral task-output commit state machine.
//!
//! Inspired by MapReduce task attempts: different executions of one logical
//! task may stage output, but only one fenced attempt can publish a result.
//! This module deliberately DOES NOT write to a database or object store.
//!
//! IMPORTANT: Callers must load, mutate, and persist this entire state using
//! one atomic compare-and-swap transaction against a durable task record.
//! A read-modify-write without CAS is NOT safe for concurrent controllers.
//! Worker identity must also be authenticated by the execution host; an
//! `AttemptToken` is not a cryptographic credential. Object bytes must be
//! uploaded immutably and verified against the staged digest before commit.
//! A commit publishes an immutable artifact reference, not external effects.

use serde::{Deserialize, Serialize};
use std::fmt;

const CHECKPOINT_VERSION: u16 = 1;

/// The owner assigned by a trusted task controller; the epoch is monotonic
/// within the logical task and MUST NOT be reused after restart.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct AttemptToken {
    pub task_key: String,
    pub attempt_id: String,
    pub epoch: u64,
}

/// Metadata for an output that has been staged, but not yet published.
/// The artifact key must refer to immutable, separately verified bytes.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct StagedTaskOutput {
    pub attempt: AttemptToken,
    pub artifact_key: String,
    pub digest: [u8; 32],
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum CommitDecision {
    /// This call made the staged result authoritative.
    Won,
    /// Exact retry of a result previously committed by this attempt.
    AlreadyWon,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum CommitError {
    InvalidTaskKey,
    InvalidAttemptId,
    InvalidArtifactKey,
    EpochExhausted,
    Fenced,
    NotStaged,
    StageConflict,
    AlreadyCommitted,
    InvalidCheckpoint(String),
}

impl fmt::Display for CommitError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::InvalidTaskKey => write!(f, "task key cannot be empty"),
            Self::InvalidAttemptId => write!(f, "attempt ID cannot be empty"),
            Self::InvalidArtifactKey => write!(f, "artifact key cannot be empty"),
            Self::EpochExhausted => write!(f, "task attempt fencing epoch exhausted"),
            Self::Fenced => write!(f, "attempt does not own the current task epoch"),
            Self::NotStaged => write!(f, "attempt has no staged result"),
            Self::StageConflict => write!(f, "attempt output differs from its first staged result"),
            Self::AlreadyCommitted => write!(f, "logical task already has an authoritative result"),
            Self::InvalidCheckpoint(reason) => write!(f, "invalid task checkpoint: {reason}"),
        }
    }
}

impl std::error::Error for CommitError {}

/// Versioned logical task state. Operations are deterministic; the owning
/// storage backend is responsible for CAS, fsync, and multi-writer exclusion.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct TaskOutputState {
    task_key: String,
    epoch: u64,
    owner: Option<AttemptToken>,
    staged: Option<StagedTaskOutput>,
    committed: Option<StagedTaskOutput>,
}

#[derive(Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
struct Checkpoint {
    version: u16,
    state: TaskOutputState,
}

impl TaskOutputState {
    pub fn new(task_key: impl Into<String>) -> Result<Self, CommitError> {
        let task_key = task_key.into();
        if task_key.trim().is_empty() {
            return Err(CommitError::InvalidTaskKey);
        }
        Ok(Self {
            task_key,
            epoch: 0,
            owner: None,
            staged: None,
            committed: None,
        })
    }

    pub fn task_key(&self) -> &str {
        &self.task_key
    }

    pub fn current_epoch(&self) -> u64 {
        self.epoch
    }

    pub fn committed(&self) -> Option<&StagedTaskOutput> {
        self.committed.as_ref()
    }

    /// Explicit ownership transition. A controller must prove prior ownership
    /// was fenced *before* atomically persisting this updated state. Heartbeat
    /// expiry or gossip suspicion alone is insufficient proof of fencing.
    pub fn assign(&mut self, attempt_id: impl Into<String>) -> Result<AttemptToken, CommitError> {
        let attempt_id = attempt_id.into();
        if attempt_id.trim().is_empty() {
            return Err(CommitError::InvalidAttemptId);
        }
        if self.committed.is_some() {
            return Err(CommitError::AlreadyCommitted);
        }
        let next_epoch = self.epoch.checked_add(1).ok_or(CommitError::EpochExhausted)?;
        let token = AttemptToken {
            task_key: self.task_key.clone(),
            attempt_id,
            epoch: next_epoch,
        };
        self.epoch = next_epoch;
        self.owner = Some(token.clone());
        // A previous attempt's staging is never transferable to its successor.
        self.staged = None;
        Ok(token)
    }

    fn require_owner(&self, token: &AttemptToken) -> Result<(), CommitError> {
        if self.owner.as_ref() == Some(token) && token.epoch != 0 && token.task_key == self.task_key {
            Ok(())
        } else {
            Err(CommitError::Fenced)
        }
    }

    /// Stage an immutable output reference. Repeating the exact same stage is
    /// idempotent, but a single attempt cannot change its selected bytes/key.
    pub fn stage(
        &mut self,
        token: &AttemptToken,
        artifact_key: impl Into<String>,
        digest: [u8; 32],
    ) -> Result<(), CommitError> {
        let artifact_key = artifact_key.into();
        self.require_owner(token)?;
        if self.committed.is_some() {
            return Err(CommitError::AlreadyCommitted);
        }
        if artifact_key.trim().is_empty() {
            return Err(CommitError::InvalidArtifactKey);
        }
        let proposal = StagedTaskOutput {
            attempt: token.clone(),
            artifact_key,
            digest,
        };
        if let Some(existing) = &self.staged {
            if existing == &proposal {
                return Ok(());
            }
            return Err(CommitError::StageConflict);
        }
        self.staged = Some(proposal);
        Ok(())
    }

    /// Commit the exact previously staged output. No external object copy or
    /// publication is performed here. The CAS persistence transaction is the
    /// sole authoritative commit point.
    pub fn commit(
        &mut self,
        token: &AttemptToken,
        digest: [u8; 32],
    ) -> Result<CommitDecision, CommitError> {
        if let Some(committed) = &self.committed {
            return if &committed.attempt == token && committed.digest == digest {
                Ok(CommitDecision::AlreadyWon)
            } else {
                Err(CommitError::AlreadyCommitted)
            };
        }
        self.require_owner(token)?;
        let staged = self.staged.as_ref().ok_or(CommitError::NotStaged)?;
        if &staged.attempt != token || staged.digest != digest {
            return Err(CommitError::StageConflict);
        }
        self.committed = Some(staged.clone());
        Ok(CommitDecision::Won)
    }

    /// Serialize the complete logical state for a single atomic CAS commit.
    /// This API alone does not provide persistence or safe multi-process writes.
    pub fn checkpoint(&self) -> Result<Vec<u8>, CommitError> {
        serde_json::to_vec(&Checkpoint {
            version: CHECKPOINT_VERSION,
            state: self.clone(),
        })
        .map_err(|error| CommitError::InvalidCheckpoint(error.to_string()))
    }

    /// Reject unknown versions and structurally inconsistent checkpoints;
    /// never infer a winner or reset fencing epochs after corrupt recovery.
    pub fn restore(bytes: &[u8]) -> Result<Self, CommitError> {
        let checkpoint: Checkpoint = serde_json::from_slice(bytes)
            .map_err(|error| CommitError::InvalidCheckpoint(error.to_string()))?;
        if checkpoint.version != CHECKPOINT_VERSION {
            return Err(CommitError::InvalidCheckpoint(format!(
                "unsupported version {}",
                checkpoint.version
            )));
        }
        checkpoint.state.validate_recovered()?;
        Ok(checkpoint.state)
    }

    fn validate_recovered(&self) -> Result<(), CommitError> {
        let bad = |reason: &str| CommitError::InvalidCheckpoint(reason.into());
        if self.task_key.trim().is_empty() {
            return Err(bad("empty task key"));
        }
        match &self.owner {
            None => {
                if self.epoch != 0 || self.staged.is_some() || self.committed.is_some() {
                    return Err(bad("unowned state has nonzero epoch or output"));
                }
            }
            Some(owner) => {
                if owner.epoch == 0
                    || owner.epoch != self.epoch
                    || owner.attempt_id.trim().is_empty()
                    || owner.task_key != self.task_key
                {
                    return Err(bad("invalid current attempt ownership"));
                }
                if let Some(staged) = &self.staged {
                    if staged.attempt != *owner || staged.artifact_key.trim().is_empty() {
                        return Err(bad("staged output does not belong to current owner"));
                    }
                }
                if let Some(committed) = &self.committed {
                    if self.staged.as_ref() != Some(committed) {
                        return Err(bad("committed result does not match staged output"));
                    }
                }
            }
        }
        Ok(())
    }
}
