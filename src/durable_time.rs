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
use std::fmt;

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
            Some(closed) => {
                timestamp.physical_micros() < closed.physical_micros()
                    || (timestamp.physical_micros() == closed.physical_micros()
                        && timestamp.logical() <= closed.logical())
            }
            None => false,
        }
    }
}
