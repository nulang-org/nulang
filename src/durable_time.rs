//! Causal time metadata for durable transitions and safe prefix reads.
//!
//! These primitives intentionally remain storage-neutral. Durable transition
//! sequence numbers and activation epochs continue to provide fencing and
//! idempotency; HLC metadata adds causal time without replacing those invariants.
//! A [`ClosedTimestamp`] is a monotonic low-water mark: a replica may serve a
//! read at timestamp `t` only when `t <= closed_timestamp`.

use crate::hlc::HlcTimestamp;
use crate::runtime::DurableTransition;
use serde::{Deserialize, Serialize};
use std::{fmt, io};

/// Causal timestamp bound to one already-fenced durable transition identity.
///
/// This value is suitable for persistence alongside a transition or for wire
/// metadata. It deliberately copies the actor/epoch/sequence fence rather than
/// depending on a process-local pointer to the transition.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash, Serialize, Deserialize)]
pub struct DurableTransitionTime {
    actor_id: u64,
    activation_epoch: u64,
    sequence: u64,
    timestamp: HlcTimestamp,
}

impl DurableTransitionTime {
    /// Bind an HLC timestamp to the authoritative identity of `transition`.
    pub const fn for_transition(transition: &DurableTransition, timestamp: HlcTimestamp) -> Self {
        Self {
            actor_id: transition.actor_id,
            activation_epoch: transition.activation_epoch,
            sequence: transition.sequence,
            timestamp,
        }
    }

    pub const fn actor_id(self) -> u64 {
        self.actor_id
    }

    pub const fn activation_epoch(self) -> u64 {
        self.activation_epoch
    }

    pub const fn sequence(self) -> u64 {
        self.sequence
    }

    pub const fn timestamp(self) -> HlcTimestamp {
        self.timestamp
    }

    /// Verify that recovered or received metadata belongs to `transition`.
    ///
    /// The sequence/epoch fence remains authoritative. HLC metadata that was
    /// persisted beside a different transition must fail closed rather than be
    /// silently attached to the caller's transition.
    pub fn validate_for(
        self,
        transition: &DurableTransition,
    ) -> Result<(), DurableTransitionTimeError> {
        if self.actor_id != transition.actor_id {
            return Err(DurableTransitionTimeError::ActorIdMismatch {
                expected: transition.actor_id,
                actual: self.actor_id,
            });
        }
        if self.activation_epoch != transition.activation_epoch {
            return Err(DurableTransitionTimeError::ActivationEpochMismatch {
                expected: transition.activation_epoch,
                actual: self.activation_epoch,
            });
        }
        if self.sequence != transition.sequence {
            return Err(DurableTransitionTimeError::SequenceMismatch {
                expected: transition.sequence,
                actual: self.sequence,
            });
        }
        Ok(())
    }
}

/// Identity mismatch detected while attaching persisted causal metadata.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum DurableTransitionTimeError {
    ActorIdMismatch { expected: u64, actual: u64 },
    ActivationEpochMismatch { expected: u64, actual: u64 },
    SequenceMismatch { expected: u64, actual: u64 },
}

impl fmt::Display for DurableTransitionTimeError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::ActorIdMismatch { expected, actual } => write!(
                f,
                "durable transition time actor mismatch: expected {expected}, got {actual}"
            ),
            Self::ActivationEpochMismatch { expected, actual } => write!(
                f,
                "durable transition time activation epoch mismatch: expected {expected}, got {actual}"
            ),
            Self::SequenceMismatch { expected, actual } => write!(
                f,
                "durable transition time sequence mismatch: expected {expected}, got {actual}"
            ),
        }
    }
}

impl std::error::Error for DurableTransitionTimeError {}

/// A durable transition paired with optional caller-supplied causal time.
///
/// This envelope establishes the compatibility and retry-identity contract
/// before storage schemas are migrated. `legacy` preserves the existing
/// transition digest byte-for-byte. When time is present, the digest is
/// domain-separated and binds both the legacy transition identity and the
/// supplied HLC metadata. No wall clock is consulted here.
#[derive(Clone, Debug)]
pub struct TimedDurableTransition {
    transition: DurableTransition,
    time: Option<DurableTransitionTime>,
}

impl TimedDurableTransition {
    /// Wrap an existing transition without causal metadata.
    pub const fn legacy(transition: DurableTransition) -> Self {
        Self {
            transition,
            time: None,
        }
    }

    /// Bind an explicitly supplied HLC timestamp to a transition.
    pub fn with_timestamp(transition: DurableTransition, timestamp: HlcTimestamp) -> Self {
        let time = DurableTransitionTime::for_transition(&transition, timestamp);
        Self {
            transition,
            time: Some(time),
        }
    }

    /// Reconstruct an envelope from separately persisted pieces.
    ///
    /// Recovery validates the duplicated fence before exposing the envelope.
    pub fn from_parts(
        transition: DurableTransition,
        time: Option<DurableTransitionTime>,
    ) -> Result<Self, DurableTransitionTimeError> {
        if let Some(time) = time {
            time.validate_for(&transition)?;
        }
        Ok(Self { transition, time })
    }

    pub const fn transition(&self) -> &DurableTransition {
        &self.transition
    }

    pub const fn time(&self) -> Option<DurableTransitionTime> {
        self.time
    }

    pub const fn timestamp(&self) -> Option<HlcTimestamp> {
        match self.time {
            Some(time) => Some(time.timestamp()),
            None => None,
        }
    }

    pub fn into_parts(self) -> (DurableTransition, Option<DurableTransitionTime>) {
        (self.transition, self.time)
    }

    /// Canonical retry identity for a transition plus optional causal time.
    ///
    /// A transition without HLC metadata delegates directly to the existing
    /// digest, preserving all legacy retry identities. Timestamped transitions
    /// add a versioned domain separator plus fixed-width big-endian metadata so
    /// distinct HLC values cannot be treated as the same retry.
    pub fn digest(&self) -> io::Result<[u8; 32]> {
        let transition_digest = self.transition.digest()?;
        let Some(time) = self.time else {
            return Ok(transition_digest);
        };

        time.validate_for(&self.transition)
            .map_err(|error| io::Error::new(io::ErrorKind::InvalidData, error))?;

        let mut hasher = blake3::Hasher::new();
        hasher.update(b"nulang:durable-transition-hlc:v1\0");
        hasher.update(&transition_digest);
        hasher.update(&time.actor_id().to_be_bytes());
        hasher.update(&time.activation_epoch().to_be_bytes());
        hasher.update(&time.sequence().to_be_bytes());
        hasher.update(&time.timestamp().physical_micros().to_be_bytes());
        hasher.update(&time.timestamp().logical().to_be_bytes());
        Ok(*hasher.finalize().as_bytes())
    }
}

/// Error returned when closed time would move backwards.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum ClosedTimestampError {
    Regression {
        current: HlcTimestamp,
        attempted: HlcTimestamp,
    },
}

impl fmt::Display for ClosedTimestampError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Regression { current, attempted } => write!(
                f,
                "closed timestamp regression from ({}, {}) to ({}, {})",
                current.physical_micros(),
                current.logical(),
                attempted.physical_micros(),
                attempted.logical()
            ),
        }
    }
}

impl std::error::Error for ClosedTimestampError {}

/// Monotonic timestamp through which a replica knows the committed prefix is closed.
///
/// `None` is intentionally distinct from `HlcTimestamp::default()`: before a
/// close point is published, no timestamped read is authorized by this value.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct ClosedTimestamp {
    value: Option<HlcTimestamp>,
}

impl ClosedTimestamp {
    pub const fn new() -> Self {
        Self { value: None }
    }

    pub const fn get(self) -> Option<HlcTimestamp> {
        self.value
    }

    /// Advance the closed prefix.
    ///
    /// Returns `Ok(true)` when the value advances and `Ok(false)` for an
    /// idempotent repeat. A regression is rejected without mutating state.
    pub fn advance(&mut self, attempted: HlcTimestamp) -> Result<bool, ClosedTimestampError> {
        match self.value {
            None => {
                self.value = Some(attempted);
                Ok(true)
            }
            Some(current) if attempted > current => {
                self.value = Some(attempted);
                Ok(true)
            }
            Some(current) if attempted == current => Ok(false),
            Some(current) => Err(ClosedTimestampError::Regression { current, attempted }),
        }
    }

    /// Whether a read at `timestamp` is inside the known-closed committed prefix.
    pub const fn permits_read(self, timestamp: HlcTimestamp) -> bool {
        match self.value {
            Some(closed) => timestamp.physical_micros() < closed.physical_micros()
                || (timestamp.physical_micros() == closed.physical_micros()
                    && timestamp.logical() <= closed.logical()),
            None => false,
        }
    }
}
