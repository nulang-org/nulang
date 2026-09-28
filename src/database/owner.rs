//! Typed tablet ownership and admission control.
//!
//! This is the actor-core state machine above WalBackedTablet. It owns the
//! write admission queue and serializes storage mutations, while the WAL/MVCC
//! hot path remains ordinary local Rust code.

use std::collections::VecDeque;
use std::fmt;
use std::path::Path;

use super::store::{WalBackedError, WalBackedTablet};
use super::tablet::{TabletError, TabletMutation, TabletWrite};

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct TabletOwnerConfig {
    pub queue_capacity: usize,
}

impl Default for TabletOwnerConfig {
    fn default() -> Self {
        Self {
            queue_capacity: 1024,
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum TabletOwnerState {
    Serving,
    Draining { next_epoch: u64 },
    Faulted,
}

/// Process-local correlation id for one admitted write.
///
/// This id is intentionally not durable and resets when the owner is reopened.
/// Network retry/idempotency keys must be a separate durable identity.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub struct TabletRequestId(u64);

impl TabletRequestId {
    pub fn get(self) -> u64 {
        self.0
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
struct AdmittedWrite {
    request_id: TabletRequestId,
    write: TabletWrite,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct TabletCommitResult {
    pub request_id: TabletRequestId,
    pub sequence: u64,
}

#[derive(Debug)]
pub struct TabletOwner {
    tablet: WalBackedTablet,
    config: TabletOwnerConfig,
    state: TabletOwnerState,
    queue: VecDeque<AdmittedWrite>,
    next_request_id: u64,
}

impl TabletOwner {
    pub fn open(
        descriptor: super::tablet::TabletDescriptor,
        wal_path: impl AsRef<Path>,
        config: TabletOwnerConfig,
    ) -> Result<Self, WalBackedError> {
        Ok(Self {
            tablet: WalBackedTablet::open(descriptor, wal_path)?,
            config,
            state: TabletOwnerState::Serving,
            queue: VecDeque::new(),
            next_request_id: 1,
        })
    }

    pub fn state(&self) -> TabletOwnerState {
        self.state
    }

    pub fn current_sequence(&self) -> Result<u64, TabletOwnerError> {
        if self.state == TabletOwnerState::Faulted {
            return Err(TabletOwnerError::Faulted);
        }
        Ok(self.tablet.current_sequence())
    }

    pub fn queued_writes(&self) -> usize {
        self.queue.len()
    }

    pub fn is_drained(&self) -> bool {
        matches!(self.state, TabletOwnerState::Draining { .. }) && self.queue.is_empty()
    }

    pub fn read_at(
        &self,
        key: &[u8],
        snapshot: u64,
    ) -> Result<Option<&[u8]>, TabletOwnerError> {
        if self.state == TabletOwnerState::Faulted {
            return Err(TabletOwnerError::Faulted);
        }
        self.tablet
            .read_at(key, snapshot)
            .map_err(TabletOwnerError::Tablet)
    }

    pub fn read_latest(&self, key: &[u8]) -> Result<Option<&[u8]>, TabletOwnerError> {
        if self.state == TabletOwnerState::Faulted {
            return Err(TabletOwnerError::Faulted);
        }
        Ok(self.tablet.read_latest(key))
    }

    pub fn admit_write(
        &mut self,
        presented_epoch: u64,
        expected_previous_sequence: u64,
        mutations: Vec<TabletMutation>,
    ) -> Result<TabletRequestId, TabletAdmissionError> {
        match self.state {
            TabletOwnerState::Serving => {}
            TabletOwnerState::Draining { next_epoch } => {
                return Err(TabletAdmissionError::Draining { next_epoch });
            }
            TabletOwnerState::Faulted => return Err(TabletAdmissionError::Faulted),
        }

        let current_epoch = self.tablet.descriptor().ownership_epoch();
        if presented_epoch < current_epoch {
            return Err(TabletAdmissionError::StaleEpoch {
                current: current_epoch,
                presented: presented_epoch,
            });
        }
        if presented_epoch > current_epoch {
            return Err(TabletAdmissionError::UnknownEpoch {
                current: current_epoch,
                presented: presented_epoch,
            });
        }

        if self.queue.len() >= self.config.queue_capacity {
            return Err(TabletAdmissionError::Backpressured {
                capacity: self.config.queue_capacity,
                queued: self.queue.len(),
            });
        }

        let admission_tail = self
            .tablet
            .current_sequence()
            .checked_add(self.queue.len() as u64)
            .ok_or(TabletAdmissionError::SequenceOverflow)?;
        let write = TabletWrite::prepare(
            self.tablet.descriptor(),
            presented_epoch,
            expected_previous_sequence,
            admission_tail,
            mutations,
        )
        .map_err(TabletAdmissionError::Tablet)?;

        let request_id = TabletRequestId(self.next_request_id);
        self.next_request_id = self
            .next_request_id
            .checked_add(1)
            .ok_or(TabletAdmissionError::RequestIdOverflow)?;
        self.queue.push_back(AdmittedWrite { request_id, write });
        Ok(request_id)
    }

    pub fn process_next(&mut self) -> Result<TabletCommitResult, TabletOwnerError> {
        if self.state == TabletOwnerState::Faulted {
            return Err(TabletOwnerError::Faulted);
        }

        let admitted = self.queue.pop_front().ok_or(TabletOwnerError::QueueEmpty)?;
        let sequence = match self.tablet.commit(admitted.write) {
            Ok(sequence) => sequence,
            Err(error) => {
                self.state = TabletOwnerState::Faulted;
                let invalidated_request_ids = self
                    .queue
                    .drain(..)
                    .map(|queued| queued.request_id)
                    .collect();
                return Err(TabletOwnerError::Storage {
                    request_id: admitted.request_id,
                    invalidated_request_ids,
                    error,
                });
            }
        };
        Ok(TabletCommitResult {
            request_id: admitted.request_id,
            sequence,
        })
    }

    pub fn begin_drain(&mut self, next_epoch: u64) -> Result<(), TabletAdmissionError> {
        match self.state {
            TabletOwnerState::Serving => {}
            TabletOwnerState::Draining { next_epoch } => {
                return Err(TabletAdmissionError::Draining { next_epoch });
            }
            TabletOwnerState::Faulted => return Err(TabletAdmissionError::Faulted),
        }

        let current = self.tablet.descriptor().ownership_epoch();
        if next_epoch <= current {
            return Err(TabletAdmissionError::EpochNotAdvanced {
                current,
                proposed: next_epoch,
            });
        }
        self.state = TabletOwnerState::Draining { next_epoch };
        Ok(())
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum TabletAdmissionError {
    Backpressured { capacity: usize, queued: usize },
    Draining { next_epoch: u64 },
    StaleEpoch { current: u64, presented: u64 },
    UnknownEpoch { current: u64, presented: u64 },
    EpochNotAdvanced { current: u64, proposed: u64 },
    SequenceOverflow,
    RequestIdOverflow,
    Faulted,
    Tablet(TabletError),
}

impl fmt::Display for TabletAdmissionError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Backpressured { capacity, queued } => write!(
                f,
                "tablet admission queue is full: {queued} queued at capacity {capacity}"
            ),
            Self::Draining { next_epoch } => {
                write!(
                    f,
                    "tablet owner is draining for ownership epoch {next_epoch}"
                )
            }
            Self::StaleEpoch { current, presented } => write!(
                f,
                "stale tablet ownership epoch {presented}; current epoch is {current}"
            ),
            Self::UnknownEpoch { current, presented } => write!(
                f,
                "unrecognized future tablet ownership epoch {presented}; current epoch is {current}"
            ),
            Self::EpochNotAdvanced { current, proposed } => write!(
                f,
                "handoff epoch {proposed} must be newer than current epoch {current}"
            ),
            Self::SequenceOverflow => f.write_str("tablet admission sequence overflow"),
            Self::RequestIdOverflow => f.write_str("tablet request id overflow"),
            Self::Faulted => {
                f.write_str("tablet owner is faulted; reopen from durable state before retrying")
            }
            Self::Tablet(error) => write!(f, "tablet admission rejected: {error}"),
        }
    }
}

impl std::error::Error for TabletAdmissionError {}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum TabletOwnerError {
    QueueEmpty,
    Faulted,
    Tablet(TabletError),
    Storage {
        request_id: TabletRequestId,
        invalidated_request_ids: Vec<TabletRequestId>,
        error: WalBackedError,
    },
}

impl fmt::Display for TabletOwnerError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::QueueEmpty => f.write_str("tablet owner admission queue is empty"),
            Self::Faulted => {
                f.write_str("tablet owner is faulted; reopen from durable state before retrying")
            }
            Self::Tablet(error) => write!(f, "tablet owner read rejected: {error}"),
            Self::Storage {
                request_id,
                invalidated_request_ids,
                error,
            } => write!(
                f,
                "tablet owner storage failure for request {}; invalidated {} queued request(s): {error}",
                request_id.get(),
                invalidated_request_ids.len()
            ),
        }
    }
}

impl std::error::Error for TabletOwnerError {}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::database::interruption::{with_interruption, StorageInterruptionPoint};
    use crate::database::tablet::{KeyRange, TabletDescriptor, TabletId, TabletMutation};
    use std::fs;
    use std::path::PathBuf;
    use std::sync::atomic::{AtomicU64, Ordering};

    static NEXT_TEST: AtomicU64 = AtomicU64::new(1);

    fn temp_wal() -> PathBuf {
        std::env::temp_dir().join(format!(
            "nulang_nudb_owner_fault_{}_{}.wal",
            std::process::id(),
            NEXT_TEST.fetch_add(1, Ordering::Relaxed)
        ))
    }

    fn descriptor() -> TabletDescriptor {
        TabletDescriptor::new(
            TabletId::new(151).unwrap(),
            KeyRange::new(b"a".to_vec(), Some(b"z".to_vec())).unwrap(),
            9,
        )
        .unwrap()
    }

    fn mutation(value: &[u8]) -> Vec<TabletMutation> {
        vec![TabletMutation::Put {
            key: b"k".to_vec(),
            value: value.to_vec(),
        }]
    }

    #[test]
    fn ambiguous_storage_failure_faults_owner_until_reopen() {
        let path = temp_wal();
        let _ = fs::remove_file(&path);
        let _ = fs::remove_file(path.with_extension("checkpoint"));

        let mut owner =
            TabletOwner::open(descriptor(), &path, TabletOwnerConfig { queue_capacity: 4 })
                .unwrap();

        let first = owner.admit_write(9, 0, mutation(b"v1")).unwrap();
        let second = owner.admit_write(9, 1, mutation(b"v2")).unwrap();

        let result = with_interruption(StorageInterruptionPoint::WalAfterSync, || {
            owner.process_next()
        });
        assert!(matches!(
            result,
            Err(TabletOwnerError::Storage {
                request_id,
                ref invalidated_request_ids,
                ..
            }) if request_id == first && invalidated_request_ids == &[second]
        ));
        assert_eq!(owner.state(), TabletOwnerState::Faulted);
        assert_eq!(owner.queued_writes(), 0);
        assert_eq!(
            owner.current_sequence().unwrap_err(),
            TabletOwnerError::Faulted
        );

        assert_eq!(
            owner.admit_write(9, 1, mutation(b"late")).unwrap_err(),
            TabletAdmissionError::Faulted
        );
        assert_eq!(owner.process_next().unwrap_err(), TabletOwnerError::Faulted);
        assert_eq!(
            owner.read_at(b"k", 0).unwrap_err(),
            TabletOwnerError::Faulted
        );
        assert_eq!(
            owner.read_latest(b"k").unwrap_err(),
            TabletOwnerError::Faulted
        );

        drop(owner);

        let reopened =
            TabletOwner::open(descriptor(), &path, TabletOwnerConfig { queue_capacity: 4 })
                .unwrap();
        assert_eq!(reopened.state(), TabletOwnerState::Serving);
        assert_eq!(reopened.current_sequence().unwrap(), 1);

        let _ = fs::remove_file(&path);
        let _ = fs::remove_file(path.with_extension("checkpoint"));
    }
}
