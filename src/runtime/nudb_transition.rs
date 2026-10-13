//! Experimental single-node adapter between Nulang durable transitions and NuDB.
//!
//! This is deliberately not a full `PersistenceStore` implementation: legacy
//! snapshot/journal writes, runtime recovery wiring, and distributed commits
//! need separate contracts. Never use it as a drop-in replacement for an
//! existing runtime persistence store.

use std::io;
use std::path::Path;

use serde::{Deserialize, Serialize};

use crate::database::store::WalBackedTablet;
use crate::database::tablet::{KeyRange, TabletDescriptor, TabletId, TabletMutation};
use crate::durable_effect_persistence::DurableEffectPersistenceRecord;

use super::{
    ActorSnapshot, DurableCommit, DurableOutboxMessage, DurableTailPosition, DurableTransition,
    EventEntry, JournalEntry, WorkflowEvent,
};

const ENVELOPE_VERSION: u16 = 1;
const TABLET_ID: u64 = 1;
const TABLET_EPOCH: u64 = 1;
const TAIL_PREFIX: u8 = b'L';
const TRANSITION_PREFIX: u8 = b'T';

/// Proof-of-contract storage for atomic durable transitions in one NuDB tablet.
///
/// Each accepted transition stores an immutable envelope and advances its
/// actor's tail in the **same WAL-backed tablet write**. Different actors
/// share the tablet's global commit sequence but have independent actor
/// sequences. Multiple writable instances must be excluded by the WAL's
/// persistent sidecar lock (PR #1440).
///
/// This is local, not replicated, and does not yet implement the runtime's
/// `PersistenceStore` trait or its legacy mutation APIs.
#[derive(Debug)]
pub struct NuDbTransitionJournal {
    tablet: WalBackedTablet,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
struct TailEnvelope {
    activation_epoch: u64,
    sequence: u64,
    digest: [u8; 32],
}

#[derive(Debug, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
struct TransitionEnvelope {
    envelope_version: u16,
    digest: [u8; 32],
    version: u16,
    actor_id: u64,
    activation_epoch: u64,
    sequence: u64,
    expected_previous_sequence: u64,
    command: Option<JournalEntry>,
    snapshot: Option<ActorSnapshot>,
    workflow_events: Vec<WorkflowEvent>,
    domain_events: Vec<EventEntry>,
    durable_effects: Vec<Vec<u8>>,
    outbox: Vec<DurableOutboxMessage>,
}

impl TransitionEnvelope {
    fn encode(transition: &DurableTransition, digest: [u8; 32]) -> io::Result<Vec<u8>> {
        let durable_effects = transition
            .durable_effects
            .iter()
            .map(|record| record.to_json().map_err(invalid_data))
            .collect::<io::Result<Vec<_>>>()?;
        let envelope = Self {
            envelope_version: ENVELOPE_VERSION,
            digest,
            version: transition.version,
            actor_id: transition.actor_id,
            activation_epoch: transition.activation_epoch,
            sequence: transition.sequence,
            expected_previous_sequence: transition.expected_previous_sequence,
            command: transition.command.clone(),
            snapshot: transition.snapshot.clone(),
            workflow_events: transition.workflow_events.clone(),
            domain_events: transition.domain_events.clone(),
            durable_effects,
            outbox: transition.outbox.clone(),
        };
        serde_json::to_vec(&envelope).map_err(invalid_data)
    }

    fn decode(bytes: &[u8]) -> io::Result<DurableTransition> {
        let envelope: Self = serde_json::from_slice(bytes).map_err(invalid_data)?;
        if envelope.envelope_version != ENVELOPE_VERSION {
            return Err(invalid_data(format!(
                "unsupported NuDB durable-transition envelope {}",
                envelope.envelope_version
            )));
        }
        let durable_effects = envelope
            .durable_effects
            .iter()
            .map(|encoded| DurableEffectPersistenceRecord::from_json(encoded).map_err(invalid_data))
            .collect::<io::Result<Vec<_>>>()?;
        let transition = DurableTransition {
            version: envelope.version,
            actor_id: envelope.actor_id,
            activation_epoch: envelope.activation_epoch,
            sequence: envelope.sequence,
            expected_previous_sequence: envelope.expected_previous_sequence,
            command: envelope.command,
            snapshot: envelope.snapshot,
            workflow_events: envelope.workflow_events,
            domain_events: envelope.domain_events,
            durable_effects,
            outbox: envelope.outbox,
        };
        if transition.digest().map_err(invalid_data)? != envelope.digest {
            return Err(invalid_data("NuDB durable-transition digest mismatch"));
        }
        Ok(transition)
    }
}

impl NuDbTransitionJournal {
    /// Open the dedicated single-node full-keyspace tablet. The WAL path must
    /// not be shared with any non-transition data or alternate descriptor.
    pub fn open(path: impl AsRef<Path>) -> io::Result<Self> {
        let descriptor = TabletDescriptor::new(
            TabletId::new(TABLET_ID).map_err(invalid_data)?,
            KeyRange::new(Vec::new(), None).map_err(invalid_data)?,
            TABLET_EPOCH,
        )
        .map_err(invalid_data)?;
        let tablet = WalBackedTablet::open(descriptor, path)
            .map_err(|error| io::Error::other(error.to_string()))?;
        Ok(Self { tablet })
    }

    /// Current global NuDB tablet sequence (not an individual actor's sequence).
    pub fn tablet_sequence(&self) -> u64 {
        self.tablet.current_sequence()
    }

    fn load_tail(&self, actor_id: u64) -> io::Result<Option<TailEnvelope>> {
        let tail: Option<TailEnvelope> = self
            .tablet
            .read_latest(&tail_key(actor_id))
            .map(|bytes| serde_json::from_slice(bytes).map_err(invalid_data))
            .transpose()?;

        match tail.as_ref() {
            Some(tail) => {
                if tail.activation_epoch == 0 || tail.sequence == 0 {
                    return Err(invalid_data("NuDB actor tail has invalid epoch or sequence"));
                }
                let stored = self
                    .load_transition(actor_id, tail.sequence)?
                    .ok_or_else(|| invalid_data("NuDB actor tail references missing transition"))?;
                if stored.activation_epoch != tail.activation_epoch
                    || stored.digest().map_err(invalid_data)? != tail.digest
                {
                    return Err(invalid_data("NuDB actor tail and transition disagree"));
                }
            }
            None => {
                // Every new actor starts at sequence 1. Never reset an actor
                // whose first transition survived but whose tail disappeared.
                if self.load_transition(actor_id, 1)?.is_some() {
                    return Err(invalid_data("NuDB actor history exists without a tail"));
                }
            }
        }
        Ok(tail)
    }

    pub fn load_tail_position(&self, actor_id: u64) -> io::Result<Option<DurableTailPosition>> {
        Ok(self.load_tail(actor_id)?.map(|tail| DurableTailPosition {
            activation_epoch: tail.activation_epoch,
            sequence: tail.sequence,
        }))
    }

    /// Audit an actor's complete acknowledged transition chain before runtime
    /// replay or publication of recovered state. A valid tail alone does not
    /// establish that every intermediate transition survived.
    ///
    /// This is an explicit O(actor sequence) recovery operation, not a
    /// per-commit hot-path check. It retains only one decoded record at a time,
    /// performs no writes, and never repairs or silently skips missing data.
    /// Callers must complete this audit before applying any recovered effects.
    pub fn verify_actor_history(
        &self,
        actor_id: u64,
    ) -> io::Result<Option<DurableTailPosition>> {
        let Some(tail) = self.load_tail(actor_id)? else {
            return Ok(None);
        };
        if tail.sequence > self.tablet.current_sequence() {
            return Err(invalid_data(
                "NuDB actor tail exceeds the tablet's committed sequence",
            ));
        }

        let mut previous_epoch = 0;
        for sequence in 1..=tail.sequence {
            let record = self
                .load_transition(actor_id, sequence)?
                .ok_or_else(|| invalid_data("NuDB actor history has a missing transition"))?;
            if record.expected_previous_sequence != sequence - 1 {
                return Err(invalid_data("NuDB actor history predecessor mismatch"));
            }
            if record.activation_epoch < previous_epoch {
                return Err(invalid_data("NuDB actor history activation epoch regressed"));
            }
            previous_epoch = record.activation_epoch;
        }
        if previous_epoch != tail.activation_epoch {
            return Err(invalid_data(
                "NuDB actor history final activation epoch disagrees with tail",
            ));
        }
        // A rewritten or stale tail must not suppress an already committed
        // next transition. This also catches orphaned next records before
        // a future actor admission would attempt to reuse that sequence.
        if let Some(next_sequence) = tail.sequence.checked_add(1) {
            if self.load_transition(actor_id, next_sequence)?.is_some() {
                return Err(invalid_data("NuDB actor transition exists beyond its tail"));
            }
        }

        Ok(Some(DurableTailPosition {
            activation_epoch: tail.activation_epoch,
            sequence: tail.sequence,
        }))
    }

    pub fn load_transition(
        &self,
        actor_id: u64,
        sequence: u64,
    ) -> io::Result<Option<DurableTransition>> {
        self.tablet
            .read_latest(&transition_key(actor_id, sequence))
            .map(|bytes| {
                let transition = TransitionEnvelope::decode(bytes)?;
                if transition.actor_id != actor_id || transition.sequence != sequence {
                    return Err(invalid_data("NuDB transition key and envelope disagree"));
                }
                Ok(transition)
            })
            .transpose()
    }

    /// Validate, serialize, and append the full transition plus its actor tail
    /// as a single durable NuDB write before publishing either key.
    pub fn commit_transition(
        &mut self,
        transition: DurableTransition,
    ) -> io::Result<DurableCommit> {
        let digest = transition.digest()?;
        let envelope = TransitionEnvelope::encode(&transition, digest)?;
        let tail = self.load_tail(transition.actor_id)?;
        if let Some(tail) = &tail {
            if transition.activation_epoch == tail.activation_epoch
                && transition.sequence == tail.sequence
            {
                if digest == tail.digest {
                    return Ok(DurableCommit {
                        actor_id: transition.actor_id,
                        activation_epoch: transition.activation_epoch,
                        sequence: transition.sequence,
                        digest,
                    });
                }
                return Err(io::Error::new(
                    io::ErrorKind::AlreadyExists,
                    "conflicting NuDB transition already committed at epoch/sequence",
                ));
            }
            if transition.activation_epoch < tail.activation_epoch {
                return Err(io::Error::new(
                    io::ErrorKind::PermissionDenied,
                    "stale NuDB actor activation epoch",
                ));
            }
        }
        let committed_sequence = tail.map(|tail| tail.sequence).unwrap_or(0);
        if transition.expected_previous_sequence != committed_sequence {
            return Err(io::Error::new(
                io::ErrorKind::InvalidData,
                format!(
                    "NuDB transition predecessor {} does not match actor tail {}",
                    transition.expected_previous_sequence, committed_sequence
                ),
            ));
        }

        let tail = TailEnvelope {
            activation_epoch: transition.activation_epoch,
            sequence: transition.sequence,
            digest,
        };
        let tail_bytes = serde_json::to_vec(&tail).map_err(invalid_data)?;
        let write = self
            .tablet
            .prepare_write(
                TABLET_EPOCH,
                self.tablet.current_sequence(),
                vec![
                    TabletMutation::Put {
                        key: transition_key(transition.actor_id, transition.sequence),
                        value: envelope,
                    },
                    TabletMutation::Put {
                        key: tail_key(transition.actor_id),
                        value: tail_bytes,
                    },
                ],
            )
            .map_err(invalid_data)?;
        self.tablet
            .commit(write)
            .map_err(|error| io::Error::other(error.to_string()))?;
        Ok(DurableCommit {
            actor_id: transition.actor_id,
            activation_epoch: transition.activation_epoch,
            sequence: transition.sequence,
            digest,
        })
    }

    /// Persist a checksummed MVCC checkpoint, then reclaim the acknowledged WAL.
    pub fn checkpoint(&mut self) -> io::Result<()> {
        self.tablet
            .checkpoint()
            .map_err(|error| io::Error::other(error.to_string()))
    }
}

fn tail_key(actor_id: u64) -> Vec<u8> {
    let mut key = Vec::with_capacity(9);
    key.push(TAIL_PREFIX);
    key.extend_from_slice(&actor_id.to_be_bytes());
    key
}

fn transition_key(actor_id: u64, sequence: u64) -> Vec<u8> {
    let mut key = Vec::with_capacity(17);
    key.push(TRANSITION_PREFIX);
    key.extend_from_slice(&actor_id.to_be_bytes());
    key.extend_from_slice(&sequence.to_be_bytes());
    key
}

fn invalid_data(error: impl std::fmt::Display) -> io::Error {
    io::Error::new(io::ErrorKind::InvalidData, error.to_string())
}
